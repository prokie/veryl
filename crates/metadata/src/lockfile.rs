use crate::git::Git;
use crate::metadata::{Dependency, Metadata};
use crate::metadata_error::MetadataError;
use crate::pubfile::{Pubfile, Release};
use crate::{utils, PathPair};
use log::info;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::str::FromStr;
use url::Url;
use uuid::Uuid;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lockfile {
    projects: Vec<Lock>,
    #[serde(skip)]
    pub lock_table: HashMap<Url, Vec<Lock>>,
    #[serde(skip)]
    force_update: bool,
    #[serde(skip)]
    modified: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    pub name: String,
    pub uuid: Uuid,
    pub version: Version,
    pub url: Url,
    pub revision: String,
    pub dependencies: Vec<LockDependency>,
    #[serde(skip)]
    used: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockDependency {
    pub name: String,
    pub version: Version,
    pub url: Url,
    pub revision: String,
}

impl Lockfile {
    pub fn load<T: AsRef<Path>>(path: T) -> Result<Self, MetadataError> {
        let path = path.as_ref().canonicalize()?;
        let text = fs::read_to_string(path)?;
        let mut ret = Self::from_str(&text)?;
        let mut locks = Vec::new();
        locks.append(&mut ret.projects);

        ret.lock_table.clear();
        for lock in locks {
            ret.lock_table
                .entry(lock.url.clone())
                .and_modify(|x| x.push(lock.clone()))
                .or_insert(vec![lock]);
        }

        Ok(ret)
    }

    pub fn save<T: AsRef<Path>>(&mut self, path: T) -> Result<(), MetadataError> {
        self.projects.clear();
        for locks in self.lock_table.values() {
            self.projects.append(&mut locks.clone());
        }
        self.projects.sort_by(|x, y| x.name.cmp(&y.name));

        let mut text = String::new();
        text.push_str("# This file is automatically @generated by Veryl.\n");
        text.push_str("# It is not intended for manual editing.\n");
        text.push_str(&toml::to_string_pretty(&self)?);
        fs::write(&path, text.as_bytes())?;
        Ok(())
    }

    pub fn new(metadata: &Metadata) -> Result<Self, MetadataError> {
        let mut ret = Lockfile::default();

        let mut name_table = HashSet::new();
        let mut uuid_table = HashSet::new();
        ret.gen_locks(metadata, &mut name_table, &mut uuid_table, true)?;

        Ok(ret)
    }

    pub fn update(
        &mut self,
        metadata: &Metadata,
        force_update: bool,
    ) -> Result<bool, MetadataError> {
        self.force_update = force_update;
        self.modified = false;

        let mut name_table = HashSet::new();
        let mut uuid_table = HashSet::new();
        for locks in self.lock_table.values_mut() {
            for lock in locks {
                name_table.insert(lock.name.clone());
                uuid_table.insert(lock.uuid);
                lock.used = false;
            }
        }
        self.gen_locks(metadata, &mut name_table, &mut uuid_table, true)?;

        // Drop unused locks
        for locks in self.lock_table.values_mut() {
            for lock in locks.iter() {
                if !lock.used {
                    info!("Removing dependency ({} @ {})", lock.url, lock.version);
                    self.modified = true;
                }
            }
            locks.retain(|x| x.used);
        }
        self.lock_table.retain(|_, x| !x.is_empty());

        Ok(self.modified)
    }

    pub fn paths(&self, base_dst: &Path) -> Result<Vec<PathPair>, MetadataError> {
        let mut ret = Vec::new();

        for locks in self.lock_table.values() {
            for lock in locks {
                let metadata = self.get_metadata(&lock.url, &lock.revision)?;
                let path = metadata.metadata_path.parent().unwrap();

                for src in &utils::gather_files_with_extension(path, "vl")? {
                    let rel = src.strip_prefix(path)?;
                    let mut dst = base_dst.join(&lock.name);
                    dst.push(rel);
                    dst.set_extension("sv");
                    ret.push(PathPair {
                        prj: lock.name.clone(),
                        src: src.to_path_buf(),
                        dst,
                    });
                }
            }
        }

        Ok(ret)
    }

    fn gen_uuid(url: &Url, revision: &str) -> Result<Uuid, MetadataError> {
        let mut url = url.as_str().to_string();
        url.push_str(revision);
        Ok(Uuid::new_v5(&Uuid::NAMESPACE_URL, url.as_bytes()))
    }

    fn gen_locks(
        &mut self,
        metadata: &Metadata,
        name_table: &mut HashSet<String>,
        uuid_table: &mut HashSet<Uuid>,
        root: bool,
    ) -> Result<(), MetadataError> {
        // breadth first search because root has top priority of name
        let mut dependencies_metadata = Vec::new();
        for (url, dep) in &metadata.dependencies {
            for (release, name) in self.resolve_dependency(url, dep)? {
                let metadata = self.get_metadata(url, &release.revision)?;
                let mut name = name.unwrap_or(metadata.project.name.clone());

                // avoid name conflict by adding suffix
                if name_table.contains(&name) {
                    if root {
                        return Err(MetadataError::NameConflict(name.clone()));
                    }
                    let mut suffix = 0;
                    loop {
                        let new_name = format!("{name}_{suffix}");
                        if !name_table.contains(&new_name) {
                            name = new_name;
                            break;
                        }
                        suffix += 1;
                    }
                }
                name_table.insert(name.clone());

                let mut dependencies = Vec::new();
                for (url, dep) in &metadata.dependencies {
                    for (release, name) in self.resolve_dependency(url, dep)? {
                        let metadata = self.get_metadata(url, &release.revision)?;
                        let name = name.unwrap_or(metadata.project.name.clone());
                        // project local name is not required to check name_table

                        let dependency = LockDependency {
                            name: name.clone(),
                            version: release.version.clone(),
                            url: url.clone(),
                            revision: release.revision.clone(),
                        };
                        dependencies.push(dependency);
                    }
                }

                let uuid = Self::gen_uuid(url, &release.revision)?;
                if !uuid_table.contains(&uuid) {
                    let lock = Lock {
                        name: name.clone(),
                        uuid,
                        version: release.version,
                        url: url.clone(),
                        revision: release.revision,
                        dependencies,
                        used: true,
                    };

                    info!("Adding dependency ({} @ {})", lock.url, lock.version);

                    self.lock_table
                        .entry(lock.url.clone())
                        .and_modify(|x| x.push(lock.clone()))
                        .or_insert(vec![lock]);
                    self.modified = true;

                    uuid_table.insert(uuid);
                    dependencies_metadata.push(metadata);
                }
            }
        }

        for metadata in dependencies_metadata {
            self.gen_locks(&metadata, name_table, uuid_table, false)?;
        }

        Ok(())
    }

    fn resolve_dependency(
        &mut self,
        url: &Url,
        dep: &Dependency,
    ) -> Result<Vec<(Release, Option<String>)>, MetadataError> {
        Ok(match dep {
            Dependency::Version(x) => {
                let release = self.resolve_version(url, x)?;
                vec![(release, None)]
            }
            Dependency::Single(x) => {
                let release = self.resolve_version(url, &x.version)?;
                vec![(release, Some(x.name.clone()))]
            }
            Dependency::Multi(x) => {
                let mut ret = Vec::new();
                for x in x {
                    let release = self.resolve_version(url, &x.version)?;
                    ret.push((release, Some(x.name.clone())));
                }
                ret
            }
        })
    }

    fn resolve_version(
        &mut self,
        url: &Url,
        version_req: &VersionReq,
    ) -> Result<Release, MetadataError> {
        if let Some(release) = self.resolve_version_from_lockfile(url, version_req)? {
            if self.force_update {
                let latest = self.resolve_version_from_latest(url, version_req)?;
                if release.version != latest.version {
                    info!(
                        "Updating dependency ({} @ {} -> {})",
                        url, release.version, latest.version
                    );
                }
                Ok(latest)
            } else {
                Ok(release)
            }
        } else {
            let latest = self.resolve_version_from_latest(url, version_req)?;
            Ok(latest)
        }
    }

    fn resolve_version_from_lockfile(
        &mut self,
        url: &Url,
        version_req: &VersionReq,
    ) -> Result<Option<Release>, MetadataError> {
        if let Some(locks) = self.lock_table.get_mut(url) {
            locks.sort_by(|a, b| b.version.cmp(&a.version));
            for lock in locks {
                if version_req.matches(&lock.version) {
                    lock.used = true;
                    let release = Release {
                        version: lock.version.clone(),
                        revision: lock.revision.clone(),
                    };
                    return Ok(Some(release));
                }
            }
        }
        Ok(None)
    }

    fn resolve_version_from_latest(
        &mut self,
        url: &Url,
        version_req: &VersionReq,
    ) -> Result<Release, MetadataError> {
        let resolve_dir = Metadata::cache_dir().join("resolve");

        if !resolve_dir.exists() {
            fs::create_dir_all(&resolve_dir)?;
        }

        let uuid = Self::gen_uuid(url, "")?;

        let path = resolve_dir.join(uuid.simple().encode_lower(&mut Uuid::encode_buffer()));
        let git = Git::clone(url, &path)?;
        git.fetch()?;
        git.checkout(None)?;

        let toml = path.join("Veryl.pub");
        let mut pubfile = Pubfile::load(&toml)?;

        pubfile.releases.sort_by(|a, b| b.version.cmp(&a.version));

        for release in &pubfile.releases {
            if version_req.matches(&release.version) {
                return Ok(release.clone());
            }
        }

        Err(MetadataError::VersionNotFound {
            url: url.clone(),
            version: version_req.to_string(),
        })
    }

    fn get_metadata(&self, url: &Url, revision: &str) -> Result<Metadata, MetadataError> {
        let dependencies_dir = Metadata::cache_dir().join("dependencies");

        if !dependencies_dir.exists() {
            fs::create_dir_all(&dependencies_dir)?;
        }

        let uuid = Self::gen_uuid(url, revision)?;

        let path = dependencies_dir.join(uuid.simple().encode_lower(&mut Uuid::encode_buffer()));
        if !path.exists() {
            let git = Git::clone(url, &path)?;
            git.fetch()?;
            git.checkout(Some(revision))?;
        }

        let toml = path.join("Veryl.toml");
        let metadata = Metadata::load(toml)?;
        Ok(metadata)
    }
}

impl FromStr for Lockfile {
    type Err = MetadataError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lockfile: Lockfile = toml::from_str(s)?;
        Ok(lockfile)
    }
}
