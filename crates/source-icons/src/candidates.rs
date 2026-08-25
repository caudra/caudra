use std::collections::HashSet;

use url::Url;

#[derive(Clone, Debug)]
pub(crate) struct FallbackCandidate {
    pub(crate) url: Url,
    pub(crate) score: i32,
}

pub(crate) fn build_fallback_candidates(
    page_url: &Url,
    max_candidates: usize,
) -> Vec<FallbackCandidate> {
    if max_candidates == 0 {
        return Vec::new();
    }
    let files = [
        ("favicon.png", 14),
        ("apple-touch-icon.png", 13),
        ("favicon.ico", 10),
        ("favicon.svg", 8),
        ("icons.ico", 6),
    ];
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    // At most `max_candidates` directory levels can contribute to the top
    // `max_candidates` scored results. Stop there instead of materializing an
    // attacker-controlled path hierarchy before the caller truncates it.
    for (depth, directory) in directory_paths(page_url, max_candidates)
        .into_iter()
        .enumerate()
    {
        for (file, bonus) in files {
            let mut url = page_url.clone();
            url.set_path(&format!("{directory}{file}"));
            url.set_query(None);
            url.set_fragment(None);
            if seen.insert(url.as_str().to_owned()) {
                candidates.push(FallbackCandidate {
                    url,
                    score: 60 + bonus - i32::try_from(depth).unwrap_or(i32::MAX).saturating_mul(4),
                });
            }
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.score));
    candidates.truncate(max_candidates);
    candidates
}

fn directory_paths(page_url: &Url, limit: usize) -> Vec<String> {
    let path = page_url.path();
    let mut starts = Vec::new();
    if path.ends_with('/') {
        starts.push(path.to_owned());
    } else {
        let last_segment = path.rsplit('/').next().unwrap_or_default();
        if !looks_like_file(last_segment) {
            starts.push(format!("{path}/"));
        }
        let parent_end = path.rfind('/').map_or(0, |index| index + 1);
        starts.push(if parent_end == 0 {
            "/".to_owned()
        } else {
            path[..parent_end].to_owned()
        });
    }

    let mut seen = HashSet::new();
    let mut output = Vec::new();
    'starts: for start in starts {
        let mut directory = start;
        loop {
            if seen.insert(directory.clone()) {
                output.push(directory.clone());
                if output.len() >= limit {
                    break 'starts;
                }
            }
            if directory == "/" {
                break;
            }
            let trimmed = directory.trim_end_matches('/');
            let parent_end = trimmed.rfind('/').unwrap_or(0);
            directory.truncate(parent_end + 1);
            if directory.is_empty() {
                directory.push('/');
            }
        }
    }
    if output.is_empty() {
        output.push("/".to_owned());
    }
    output
}

fn looks_like_file(segment: &str) -> bool {
    let Some((_, extension)) = segment.rsplit_once('.') else {
        return false;
    };
    (1..=8).contains(&extension.len()) && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensionless_routes_walk_from_nearest_directory_to_root() {
        let page = Url::parse("https://example.com/docs/guides/start?x=1").unwrap();
        let candidates = build_fallback_candidates(&page, 20);
        let urls = candidates
            .iter()
            .map(|candidate| candidate.url.as_str())
            .collect::<Vec<_>>();
        assert_eq!(urls[0], "https://example.com/docs/guides/start/favicon.png");
        assert!(urls.contains(&"https://example.com/docs/favicon.png"));
        assert!(urls.contains(&"https://example.com/favicon.ico"));
    }

    #[test]
    fn file_routes_start_at_the_parent() {
        let page = Url::parse("https://example.com/docs/feed.xml").unwrap();
        let candidates = build_fallback_candidates(&page, 20);
        assert_eq!(
            candidates[0].url.as_str(),
            "https://example.com/docs/favicon.png"
        );
    }

    #[test]
    fn deep_paths_stop_at_the_candidate_budget() {
        let path = (0..128).map(|_| "segment").collect::<Vec<_>>().join("/");
        let page = Url::parse(&format!("https://example.com/{path}/page")).unwrap();
        let candidates = build_fallback_candidates(&page, 8);
        assert_eq!(candidates.len(), 8);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.url.as_str().len() < 8 * 1024)
        );
    }
}
