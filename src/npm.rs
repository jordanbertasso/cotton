use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_recursion::async_recursion;
use cached::proc_macro::cached;
use color_eyre::{
    eyre::{ContextCompat, Result},
    Report,
};
use compact_str::{CompactString, ToCompactString};
use futures::future::try_join_all;
use indexmap::IndexMap;
use itertools::Itertools;
use node_semver::Version;
use once_cell::sync::Lazy;
use owo_colors::OwoColorize;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use tokio::sync::Semaphore;

use crate::{
    cache::Cache,
    package::{DepReq, Dist, Package},
    progress::PROGRESS_BAR,
    util::{get_node_cpu, get_node_os, CLIENT2},
};

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct RegistryResponse {
    #[serde(rename = "dist-tags")]
    pub dist_tags: FxHashMap<CompactString, CompactString>,
    pub versions: IndexMap<Version, Package>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct PlatformMap(BTreeSet<CompactString>);

impl PlatformMap {
    pub fn allowed(&self) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .filter(|x| !x.starts_with('!'))
            .map(|x| x.as_str())
    }

    pub fn blocked(&self) -> impl Iterator<Item = &str> {
        self.0.iter().filter_map(|x| x.strip_prefix('!'))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn is_supported(&self, platform: &str) -> bool {
        if self.is_empty() {
            true
        } else {
            self.allowed().any(|o| o == platform) && !self.blocked().any(|o| o == platform)
        }
    }
}

#[tracing::instrument]
pub async fn fetch_package(name: &str) -> Result<RegistryResponse, reqwest::Error> {
    static S: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(128));
    let _permit = S.acquire().await.unwrap();

    CLIENT2
        .get(format!("https://registry.npmjs.org/{}", name))
        .send()
        .await?
        .json()
        .await
}

pub async fn fetch_package_cached(name: &str) -> Result<Arc<RegistryResponse>> {
    static CACHE: Lazy<Cache<CompactString, Result<Arc<RegistryResponse>, CompactString>>> =
        Lazy::new(|| {
            Cache::new(|key: CompactString| async move {
                fetch_package(&key)
                    .await
                    .map(Arc::new)
                    .map_err(|e| e.to_compact_string())
            })
        });

    CACHE
        .get(name.to_compact_string())
        .await
        .map_err(Report::msg)
}

#[tracing::instrument]
#[cached(result)]
async fn fetch_dep_single(d: DepReq) -> Result<(Version, Package)> {
    let res = fetch_package_cached(&d.name).await?;
    let (version, package) = res
        .versions
        .iter()
        .sorted_by_key(|(v, _)| !v.is_prerelease())
        .rfind(|(v, _)| d.version.satisfies(v))
        .wrap_err("Version cannot be satisfied")?;

    Ok((version.clone(), package.clone()))
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Hash, Clone, PartialOrd, Ord)]
pub struct ExactDep {
    pub name: CompactString,
    pub version: Version,
    pub dist: Dist,
    pub deps: BTreeSet<Arc<ExactDep>>,
    pub bins: BTreeMap<CompactString, CompactString>,
}

impl ExactDep {
    pub fn remove_deps(&mut self, filter: &FxHashSet<SingleDep>) {
        self.deps = self
            .deps
            .iter()
            .cloned()
            .filter(|dep| !filter.contains(&dep.as_single()))
            .map(|dep| {
                let mut dep = (*dep).clone();
                dep.remove_deps(filter);
                Arc::new(dep)
            })
            .collect();
    }

    pub fn id(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }

    pub fn tar(&self) -> String {
        format!("{}.tar", self.id())
    }

    pub fn tar_part(&self) -> String {
        format!("{}.tar.part", self.id())
    }

    pub fn as_single(&self) -> SingleDep {
        SingleDep {
            name: self.name.to_compact_string(),
            version: self.version.clone(),
        }
    }
}

impl Debug for ExactDep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExactDep")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("deps", &self.deps)
            .finish()
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SingleDep {
    name: CompactString,
    version: Version,
}

#[async_recursion]
pub async fn fetch_dep(d: &DepReq, stack: &[(DepReq, Version)]) -> Result<Option<Arc<ExactDep>>> {
    if stack
        .iter()
        .any(|(d2, v)| d.name == d2.name && d.version.satisfies(v))
    {
        return Ok(None);
    }

    let (version, package) = fetch_dep_single(d.clone()).await?;

    if !package.os.is_supported(get_node_os()) || !package.cpu.is_supported(get_node_cpu()) {
        if d.optional {
            return Ok(None);
        } else {
            return Err(Report::msg("Required dependency is not supported"));
        }
    }

    let deps = try_join_all(package.iter().map(|d2| {
        let version = version.clone();
        async move {
            fetch_dep_cached(
                d2,
                stack
                    .iter()
                    .cloned()
                    .chain([(d.clone(), version)])
                    .collect_vec(),
            )
            .await
        }
    }))
    .await?
    .into_iter()
    .flatten();

    PROGRESS_BAR.set_message(format!("fetched {}", d.name.bright_blue()));

    Ok(Some(Arc::new(ExactDep {
        name: d.name.to_compact_string(),
        version: version.to_owned(),
        deps: deps.into_iter().collect(),
        dist: package.dist.clone(),
        bins: package.bins(),
    })))
}

pub async fn fetch_dep_cached(
    d: DepReq,
    stack: Vec<(DepReq, Version)>,
) -> Result<Option<Arc<ExactDep>>> {
    type Args = (DepReq, Vec<(DepReq, Version)>);
    type Output = Option<Arc<ExactDep>>;

    static CACHE: Lazy<Cache<Args, Result<Output, CompactString>>> = Lazy::new(|| {
        Cache::new(|(d, stack): Args| async move {
            fetch_dep(&d, &stack)
                .await
                .map_err(|e| e.to_compact_string())
        })
    });

    CACHE
        .get((d, stack))
        .await
        .map_err(|e| Report::msg(e.to_compact_string()))
}

fn flatten_dep(dep: &ExactDep, set: &mut BTreeSet<ExactDep>) {
    if set.insert(dep.clone()) {
        for dep in &dep.deps {
            flatten_dep(dep, set)
        }
    }
}

pub fn flatten_deps<'a>(deps: impl Iterator<Item = &'a ExactDep>) -> BTreeSet<ExactDep> {
    let mut set = BTreeSet::default();
    for dep in deps {
        flatten_dep(dep, &mut set);
    }
    set
}
