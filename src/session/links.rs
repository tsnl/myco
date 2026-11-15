//! Structured PR and worktree links on a session, deduplicated by PR URL or
//! worktree host and path.

use super::Session;

/// Structured association stored on a session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionLink {
    GitHubPr {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        number: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Worktree {
        /// Harness host name (`local`, `devbox`, …).
        host: String,
        /// Absolute path on that host.
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LinkCounts {
    pub prs: usize,
    pub worktrees: usize,
}

impl LinkCounts {
    pub fn from_links(links: &[SessionLink]) -> Self {
        let mut c = Self::default();
        for link in links {
            match link {
                SessionLink::GitHubPr { .. } => c.prs += 1,
                SessionLink::Worktree { .. } => c.worktrees += 1,
            }
        }
        c
    }

    pub fn is_empty(self) -> bool {
        self.prs == 0 && self.worktrees == 0
    }
}

impl Session {
    /// Insert or update a link (dedup by PR URL or worktree host+path).
    pub fn upsert_link(&mut self, mut link: SessionLink) -> Result<(), String> {
        validate_link(&link)?;
        match &mut link {
            SessionLink::GitHubPr {
                url, repo, number, ..
            } => {
                let url_key = normalize_pr_url(url)?;
                let (parsed_repo, parsed_num) = parse_pr_fields(&url_key);
                *url = url_key.clone();
                if repo.is_none() {
                    *repo = parsed_repo;
                }
                if number.is_none() {
                    *number = parsed_num;
                }
                if let Some(existing) = self.links.iter_mut().find_map(|l| match l {
                    SessionLink::GitHubPr { url, .. } if urls_equal(url, &url_key) => Some(l),
                    _ => None,
                }) {
                    *existing = link;
                } else {
                    self.links.push(link);
                }
            }
            SessionLink::Worktree { host, path, .. } => {
                *host = host.trim().to_string();
                *path = path.trim().to_string();
                let host_key = host.clone();
                let path_key = path.clone();
                if let Some(existing) = self.links.iter_mut().find_map(|l| match l {
                    SessionLink::Worktree { host, path, .. }
                        if host == &host_key && path == &path_key =>
                    {
                        Some(l)
                    }
                    _ => None,
                }) {
                    *existing = link;
                } else {
                    self.links.push(link);
                }
            }
        }
        Ok(())
    }

    pub fn remove_link_at(&mut self, index: usize) -> Result<SessionLink, String> {
        if index >= self.links.len() {
            return Err(format!(
                "link index {index} out of range ({} links)",
                self.links.len()
            ));
        }
        Ok(self.links.remove(index))
    }

    pub fn remove_link_matching(
        &mut self,
        url: Option<&str>,
        host: Option<&str>,
        path: Option<&str>,
    ) -> Result<SessionLink, String> {
        let idx = self
            .links
            .iter()
            .position(|l| match l {
                SessionLink::GitHubPr {
                    url: existing_url, ..
                } => url.map(|u| urls_equal(existing_url, u)).unwrap_or(false),
                SessionLink::Worktree {
                    host: h, path: p, ..
                } => {
                    let host_ok = host.map(|x| x == h.as_str()).unwrap_or(false);
                    let path_ok = path.map(|x| x == p.as_str()).unwrap_or(true);
                    host_ok && path_ok
                }
            })
            .ok_or_else(|| "no matching link".to_string())?;
        Ok(self.links.remove(idx))
    }
}

// ---------------------------------------------------------------------------
// Link validation / PR URL helpers
// ---------------------------------------------------------------------------

fn validate_link(link: &SessionLink) -> Result<(), String> {
    match link {
        SessionLink::GitHubPr { url, .. } => {
            normalize_pr_url(url)?;
            Ok(())
        }
        SessionLink::Worktree { host, path, .. } => {
            if host.trim().is_empty() {
                return Err("worktree host must be non-empty".into());
            }
            let path = path.trim();
            if path.is_empty() {
                return Err("worktree path must be non-empty".into());
            }
            // Allow Unix absolute and Windows drive paths; reject relative.
            let windows_abs = path.len() >= 3 && path.as_bytes()[1] == b':';
            if !path.starts_with('/') && !windows_abs {
                return Err("worktree path must be absolute".into());
            }
            Ok(())
        }
    }
}

/// Normalize a GitHub PR reference to an https URL.
///
/// Accepts:
/// - `https://github.com/org/repo/pull/123`
/// - `http://github.com/org/repo/pull/123`
/// - `github.com/org/repo/pull/123`
/// - `org/repo#123` / `org/repo/pull/123`
pub fn normalize_pr_url(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("PR url must be non-empty".into());
    }

    // org/repo#123
    if let Some((repo, num)) = s.split_once('#')
        && repo.contains('/')
        && !repo.contains("://")
        && num.chars().all(|c| c.is_ascii_digit())
    {
        let num: u32 = num
            .parse()
            .map_err(|_| format!("invalid PR number in {s:?}"))?;
        if num == 0 {
            return Err("PR number must be > 0".into());
        }
        return Ok(format!("https://github.com/{repo}/pull/{num}"));
    }

    let mut url = s.to_string();
    if url.starts_with("github.com/") {
        url = format!("https://{url}");
    }
    if url.starts_with("http://") {
        url = format!("https://{}", &url["http://".len()..]);
    }

    // org/repo/pull/123
    if !url.contains("://")
        && let Some((repo, rest)) = url.split_once("/pull/")
        && repo.contains('/')
        && rest.chars().all(|c| c.is_ascii_digit())
    {
        url = format!("https://github.com/{repo}/pull/{rest}");
    }

    let rest = url.strip_prefix("https://github.com/").ok_or_else(|| {
        format!("PR url must be a github.com pull request URL or org/repo#N (got {raw:?})")
    })?;
    let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
    // org/repo/pull/N
    if parts.len() >= 4
        && parts[2] == "pull"
        && let Ok(n) = parts[3].parse::<u32>()
        && n > 0
    {
        return Ok(format!(
            "https://github.com/{}/{}/pull/{n}",
            parts[0], parts[1]
        ));
    }
    Err(format!(
        "PR url must be a github.com pull request URL or org/repo#N (got {raw:?})"
    ))
}

pub fn parse_pr_fields(url: &str) -> (Option<String>, Option<u32>) {
    let Ok(norm) = normalize_pr_url(url) else {
        return (None, None);
    };
    let rest = norm.trim_start_matches("https://github.com/");
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() >= 4 && parts[2] == "pull" {
        let repo = format!("{}/{}", parts[0], parts[1]);
        let number = parts[3].parse().ok();
        return (Some(repo), number);
    }
    (None, None)
}

fn urls_equal(a: &str, b: &str) -> bool {
    match (normalize_pr_url(a), normalize_pr_url(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.trim() == b.trim(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_pr_url_variants() {
        assert_eq!(
            normalize_pr_url("https://github.com/foo/bar/pull/12").unwrap(),
            "https://github.com/foo/bar/pull/12"
        );
        assert_eq!(
            normalize_pr_url("foo/bar#99").unwrap(),
            "https://github.com/foo/bar/pull/99"
        );
        assert_eq!(
            normalize_pr_url("github.com/foo/bar/pull/3").unwrap(),
            "https://github.com/foo/bar/pull/3"
        );
        assert!(normalize_pr_url("https://gitlab.com/x/y/merge_requests/1").is_err());
    }

    #[test]
    fn link_dedup_pr_and_worktree() {
        let mut s = Session::new("claude-haiku-4-5");
        s.upsert_link(SessionLink::GitHubPr {
            url: "foo/bar#1".into(),
            repo: None,
            number: None,
            note: Some("a".into()),
        })
        .unwrap();
        s.upsert_link(SessionLink::GitHubPr {
            url: "https://github.com/foo/bar/pull/1".into(),
            repo: Some("foo/bar".into()),
            number: Some(1),
            note: Some("b".into()),
        })
        .unwrap();
        assert_eq!(s.links.len(), 1);
        match &s.links[0] {
            SessionLink::GitHubPr { note, .. } => assert_eq!(note.as_deref(), Some("b")),
            _ => panic!("expected pr"),
        }

        s.upsert_link(SessionLink::Worktree {
            host: "local".into(),
            path: "/tmp/wt".into(),
            branch: Some("feat/x".into()),
            note: None,
        })
        .unwrap();
        s.upsert_link(SessionLink::Worktree {
            host: "local".into(),
            path: "/tmp/wt".into(),
            branch: Some("feat/y".into()),
            note: Some("upd".into()),
        })
        .unwrap();
        assert_eq!(s.links.len(), 2);
        match &s.links[1] {
            SessionLink::Worktree { branch, note, .. } => {
                assert_eq!(branch.as_deref(), Some("feat/y"));
                assert_eq!(note.as_deref(), Some("upd"));
            }
            _ => panic!("expected worktree"),
        }
    }
}
