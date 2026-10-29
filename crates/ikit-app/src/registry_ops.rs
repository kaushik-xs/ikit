//! Registry operations that talk to image registries: pinning a version to its image digest,
//! and syncing versions from an image's tags (OCI distribution API).

use std::time::Duration;

use ikit_core::version::parse_version;
use semver::Version;
use serde::{Deserialize, Serialize};

use crate::registry::AddVersion;
use crate::{App, AppError, Result, VersionChanges};

const INSPECT_TIMEOUT: Duration = Duration::from_secs(120);
/// Safety limit when following tag-list pagination.
const MAX_TAG_PAGES: usize = 20;

#[derive(Debug, Clone, Serialize)]
pub struct SyncReport {
    pub repository: String,
    pub tags_seen: usize,
    /// Versions newer than the newest one in the registry, newest first: (version, image).
    pub new_versions: Vec<(Version, String)>,
    pub added: bool,
}

/// An image reference split into registry, repository and tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// API host, e.g. `registry-1.docker.io` or `ghcr.io`.
    pub registry: String,
    /// e.g. `library/postgres`, `acme/orders-api`.
    pub repository: String,
    pub tag: Option<String>,
}

impl ImageRef {
    pub fn parse(image: &str) -> Self {
        let without_digest = image.split('@').next().unwrap_or(image);
        let (name, tag) = match without_digest.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => (n, Some(t.to_owned())),
            _ => (without_digest, None),
        };
        let (registry, repo) = match name.split_once('/') {
            Some((first, rest))
                if first.contains('.') || first.contains(':') || first == "localhost" =>
            {
                (first.to_owned(), rest.to_owned())
            }
            _ => ("docker.io".to_owned(), name.to_owned()),
        };
        if registry == "docker.io" {
            let repo = if repo.contains('/') {
                repo
            } else {
                format!("library/{repo}")
            };
            return Self {
                registry: "registry-1.docker.io".into(),
                repository: repo,
                tag,
            };
        }
        Self {
            registry,
            repository: repo,
            tag,
        }
    }

    /// `repo:tag` as users write it (`postgres:17.3`, `ghcr.io/acme/api:1.5.0`).
    pub fn with_tag(original: &str, tag: &str) -> String {
        let name = original.split('@').next().unwrap_or(original);
        let base = match name.rsplit_once(':') {
            Some((n, t)) if !t.contains('/') => n,
            _ => name,
        };
        format!("{base}:{tag}")
    }
}

/// Parses `Bearer realm="…",service="…",scope="…"`.
fn parse_challenge(header: &str) -> Option<(String, Vec<(String, String)>)> {
    let rest = header.strip_prefix("Bearer ")?;
    let mut realm = None;
    let mut params = Vec::new();
    for part in rest.split(',') {
        let (k, v) = part.trim().split_once('=')?;
        let v = v.trim_matches('"').to_owned();
        if k == "realm" {
            realm = Some(v);
        } else {
            params.push((k.to_owned(), v));
        }
    }
    Some((realm?, params))
}

fn next_link(header: Option<&reqwest::header::HeaderValue>) -> Option<String> {
    let value = header?.to_str().ok()?;
    let start = value.find('<')? + 1;
    let end = value.find('>')?;
    value
        .contains("rel=\"next\"")
        .then(|| value[start..end].to_owned())
}

impl App {
    /// Resolves the image digest of `service@version` with `docker buildx imagetools` and
    /// stores it, so deploys use `image@digest`.
    pub async fn registry_pin(&self, service: &str, version: &Version) -> Result<String> {
        let entry = self.registry_show(service).await?;
        let v = entry
            .version(version)
            .ok_or_else(|| AppError::NotFound(format!("{service}@{version}")))?;
        let args: Vec<String> = [
            "buildx",
            "imagetools",
            "inspect",
            &v.image,
            "--format",
            "{{json .Manifest}}",
        ]
        .map(str::to_owned)
        .to_vec();
        let out = self
            .exec
            .run("docker", &args, None, INSPECT_TIMEOUT)
            .await?;
        if !out.success() {
            return Err(AppError::Invalid(format!(
                "inspecting {} failed: {}",
                v.image,
                out.failure_detail()
            )));
        }
        #[derive(Deserialize)]
        struct Manifest {
            digest: String,
        }
        let manifest: Manifest = serde_json::from_str(out.stdout.trim()).map_err(|e| {
            AppError::Invalid(format!(
                "unexpected output from docker buildx imagetools: {e}"
            ))
        })?;
        if !manifest.digest.starts_with("sha256:") {
            return Err(AppError::Invalid(format!(
                "unexpected digest `{}`",
                manifest.digest
            )));
        }
        let changes = VersionChanges {
            digest: Some(Some(manifest.digest.clone())),
            ..Default::default()
        };
        self.registry_version_update(service, version, changes)
            .await?;
        Ok(manifest.digest)
    }

    /// Services of `workload` that must be pinned before deploying (its env is in the org's
    /// `require_digest_envs`) but are not.
    pub(crate) async fn unpinned_for_env(&self, workload: &str) -> Result<Vec<String>> {
        let wl = self.workload_get(workload).await?;
        if !self.config()?.require_digest_envs.contains(&wl.env) {
            return Ok(vec![]);
        }
        Ok(self
            .services_list(workload)
            .await?
            .into_iter()
            .filter(|s| s.digest.is_none() && !s.image.contains('@'))
            .map(|s| format!("{} ({}@{})", s.name, s.registry_service, s.version))
            .collect())
    }

    /// Tags of an image repository, via the OCI distribution API (anonymous, or with the
    /// stored registry login).
    async fn list_tags(&self, image: &ImageRef) -> Result<Vec<String>> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("ikit/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| AppError::Invalid(e.to_string()))?;
        let login_host = if image.registry == "registry-1.docker.io" {
            "docker.io"
        } else {
            image.registry.as_str()
        };
        let login = self
            .registry_logins()
            .await?
            .into_iter()
            .find(|l| l.host == login_host);
        let basic = match &login {
            Some(l) => Some((l.username.clone(), self.registry_password(&l.host)?)),
            None => None,
        };
        let http = |e: reqwest::Error| {
            AppError::Invalid(format!(
                "talking to {}: {}",
                image.registry,
                e.without_url()
            ))
        };

        let mut url = format!(
            "https://{}/v2/{}/tags/list?n=1000",
            image.registry, image.repository
        );
        let mut token: Option<String> = None;
        let mut tags = Vec::new();
        for _ in 0..MAX_TAG_PAGES {
            let mut req = client.get(&url);
            if let Some(t) = &token {
                req = req.bearer_auth(t);
            }
            let resp = req.send().await.map_err(http)?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && token.is_none() {
                let challenge = resp
                    .headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .and_then(parse_challenge)
                    .ok_or_else(|| {
                        AppError::Invalid(format!(
                            "{} wants authentication ikit cannot do",
                            image.registry
                        ))
                    })?;
                let (realm, params) = challenge;
                let mut treq = client.get(&realm).query(&params);
                if let Some((user, pass)) = &basic {
                    treq = treq.basic_auth(user, Some(pass));
                }
                // Docker Hub sends both `token` and `access_token`; others send one of them.
                #[derive(Deserialize)]
                struct Token {
                    token: Option<String>,
                    access_token: Option<String>,
                }
                let t: Token = treq
                    .send()
                    .await
                    .map_err(http)?
                    .error_for_status()
                    .map_err(http)?
                    .json()
                    .await
                    .map_err(http)?;
                token = Some(t.token.or(t.access_token).ok_or_else(|| {
                    AppError::Invalid(format!("{} returned no token", image.registry))
                })?);
                continue;
            }
            let resp = resp.error_for_status().map_err(http)?;
            let next = next_link(resp.headers().get("link"));
            #[derive(Deserialize)]
            struct TagList {
                #[serde(default)]
                tags: Option<Vec<String>>,
            }
            let page: TagList = resp.json().await.map_err(http)?;
            tags.extend(page.tags.unwrap_or_default());
            match next {
                Some(path) if path.starts_with('/') => {
                    url = format!("https://{}{path}", image.registry)
                }
                Some(full) => url = full,
                None => break,
            }
        }
        Ok(tags)
    }

    /// Finds image tags newer than the newest registry version of `service` (semver-looking
    /// tags only; prereleases with `include_pre`). With `add`, adds them as versions that copy
    /// the newest version's spec.
    pub async fn registry_sync(
        &self,
        service: &str,
        add: bool,
        include_pre: bool,
    ) -> Result<SyncReport> {
        let entry = self.registry_show(service).await?;
        let newest = entry
            .versions
            .first()
            .ok_or_else(|| AppError::Invalid(format!("{service} has no versions")))?;
        let image = ImageRef::parse(&newest.image);
        let tags = self.list_tags(&image).await?;
        let mut found: Vec<(Version, String)> = Vec::new();
        for tag in &tags {
            // Only plain version tags: `17.2`, `v1.4.2`, `1.4.2-rc.1`; not `17-alpine`.
            let Ok(v) = parse_version(tag) else { continue };
            if !v.build.is_empty() || (!v.pre.is_empty() && !include_pre) || tag.contains('_') {
                continue;
            }
            if v > newest.version
                && entry.version(&v).is_none()
                && !found.iter().any(|(f, _)| *f == v)
            {
                found.push((v, ImageRef::with_tag(&newest.image, tag)));
            }
        }
        found.sort_by(|a, b| b.0.cmp(&a.0));
        if add {
            for (version, img) in found.iter().rev() {
                let req = AddVersion {
                    version: version.clone(),
                    image: img.clone(),
                    digest: None,
                    release_notes: Some("added by `ikit registry sync`".into()),
                    spec: None,
                    copy_from: Some(newest.version.clone()),
                };
                self.registry_version_add(service, req).await?;
            }
        }
        Ok(SyncReport {
            repository: format!("{}/{}", image.registry, image.repository),
            tags_seen: tags.len(),
            new_versions: found,
            added: add,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_image_references() {
        let r = ImageRef::parse("postgres:16.6");
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str(), r.tag.as_deref()),
            ("registry-1.docker.io", "library/postgres", Some("16.6"))
        );
        let r = ImageRef::parse("minio/minio:RELEASE.2024");
        assert_eq!(r.repository, "minio/minio");
        let r = ImageRef::parse("ghcr.io/acme/orders-api:1.4.2@sha256:abc");
        assert_eq!(
            (r.registry.as_str(), r.repository.as_str(), r.tag.as_deref()),
            ("ghcr.io", "acme/orders-api", Some("1.4.2"))
        );
        let r = ImageRef::parse("localhost:5000/api");
        assert_eq!((r.registry.as_str(), r.tag), ("localhost:5000", None));
        assert_eq!(
            ImageRef::with_tag("localhost:5000/api:1", "2"),
            "localhost:5000/api:2"
        );
        assert_eq!(ImageRef::with_tag("postgres", "17"), "postgres:17");
    }

    #[test]
    fn parses_auth_challenges_and_links() {
        let (realm, params) = parse_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/postgres:pull""#,
        )
        .unwrap();
        assert_eq!(realm, "https://auth.docker.io/token");
        assert_eq!(
            params[1],
            (
                "scope".to_owned(),
                "repository:library/postgres:pull".to_owned()
            )
        );
        let link = reqwest::header::HeaderValue::from_static(
            r#"</v2/library/postgres/tags/list?last=17&n=1000>; rel="next""#,
        );
        assert_eq!(
            next_link(Some(&link)).as_deref(),
            Some("/v2/library/postgres/tags/list?last=17&n=1000")
        );
    }
}
