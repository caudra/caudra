//! Reddit serves its HTML pages behind a script challenge, so they are read
//! from the Atom feed every page offers, and from the JSON API when the feed is
//! refused. Both renderers turn the response into the same Markdown or text.

use chrono::{DateTime, Utc};
use dom_query::Document;
use http::StatusCode;
use roxmltree::Node;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use super::{
    Render, RenderContext, SiteAttempt, SitePage, SitePlan, SiteRenderError, SiteRule, site_headers,
};
use crate::html::html_fragment;
use crate::types::WebfetchFormat;

const ORIGIN: &str = "https://www.reddit.com";
const HOST: &str = "reddit.com";
const SUBDOMAIN_SUFFIX: &str = ".reddit.com";
const SHORT_HOST: &str = "redd.it";
const ICON: &str = "https://www.redditstatic.com/desktop2x/img/favicon/favicon-32x32.png";
const USER_AGENT: &str = concat!("workcell:webfetch:", env!("CARGO_PKG_VERSION"));
const RSS_ACCEPT: &str = "application/atom+xml";
const JSON_ACCEPT: &str = "application/json";
const RSS_METHOD: &str = "reddit-rss";
const JSON_METHOD: &str = "reddit-json";
const RSS: &str = "Reddit feed";
const JSON: &str = "Reddit API";
/// The API shares the feed's rate limit, so only a refusal moves on to it.
const RSS_FALLBACK_ON: &[StatusCode] = &[StatusCode::FORBIDDEN];
const KEPT_QUERY_KEYS: &[&str] = &["t", "sort", "q", "restrict_sr", "limit"];
const LIMIT_KEY: &str = "limit";
const THREAD_LIMIT: &str = "100";
const RAW_JSON: (&str, &str) = ("raw_json", "1");
const SORTS: &[&str] = &["best", "hot", "new", "top", "rising", "controversial"];
const USER_TABS: &[&str] = &["overview", "submitted", "comments"];
const FEED_EXTENSIONS: &[&str] = &[".rss", ".json"];
const POST_PREFIX: &str = "t3_";
const COMMENT_PREFIX: &str = "t1_";
const LINK_LABEL: &str = "[link]";
const UNNAMED_POST: &str = "a post";
const MAX_COMMENT_DEPTH: usize = 8;
const MAX_EXCERPT_CHARS: usize = 280;
const DATE_FORMAT: &str = "%Y-%m-%d %H:%M UTC";
const SEPARATOR: &str = " · ";
const LIST_INDENT: usize = 2;
const NUMBERED_INDENT: usize = 3;

pub(super) struct Reddit;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Shape {
    Thread,
    Listing,
}

impl SiteRule for Reddit {
    fn plan(&self, url: &Url, format: WebfetchFormat) -> Option<SitePlan> {
        if format == WebfetchFormat::Html {
            return None;
        }
        let (path, shape) = target(url)?;
        let mut query = url
            .query_pairs()
            .filter(|(key, _)| KEPT_QUERY_KEYS.contains(&key.as_ref()))
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        if shape == Shape::Thread && !query.iter().any(|(key, _)| key == LIMIT_KEY) {
            query.push((LIMIT_KEY.to_owned(), THREAD_LIMIT.to_owned()));
        }
        let (render_rss, render_json): (Render, Render) = match shape {
            Shape::Thread => (thread_rss, thread_json),
            Shape::Listing => (listing_rss, listing_json),
        };
        let mut json_query = vec![(RAW_JSON.0.to_owned(), RAW_JSON.1.to_owned())];
        json_query.extend(query.iter().cloned());
        Some(SitePlan {
            attempts: vec![
                SiteAttempt {
                    url: endpoint(&path, "rss", &query)?,
                    headers: site_headers(format, USER_AGENT, RSS_ACCEPT),
                    fallback_on: RSS_FALLBACK_ON,
                    render: Some(render_rss),
                },
                SiteAttempt {
                    url: endpoint(&path, "json", &json_query)?,
                    headers: site_headers(format, USER_AGENT, JSON_ACCEPT),
                    fallback_on: &[],
                    render: Some(render_json),
                },
            ],
            icon: Some(ICON),
        })
    }
}

fn endpoint(path: &str, extension: &str, query: &[(String, String)]) -> Option<Url> {
    let mut url = Url::parse(&format!("{ORIGIN}{path}/.{extension}")).ok()?;
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    Some(url)
}

/// The canonical path of the page `url` names, or `None` for anything this
/// rule does not know how to read.
fn target(url: &Url) -> Option<(String, Shape)> {
    let host = url.host_str()?;
    let mut segments = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if let Some(last) = segments.last_mut() {
        let segment = *last;
        *last = FEED_EXTENSIONS
            .iter()
            .find_map(|extension| segment.strip_suffix(extension))
            .unwrap_or(segment);
        if last.is_empty() {
            segments.pop();
        }
    }
    if host == SHORT_HOST {
        return match segments.as_slice() {
            [id] if is_id(id) => Some((format!("/comments/{id}"), Shape::Thread)),
            _ => None,
        };
    }
    if host != HOST && !host.ends_with(SUBDOMAIN_SUFFIX) {
        return None;
    }
    let listing = |path: String| Some((path, Shape::Listing));
    match segments.as_slice() {
        [] => listing(String::new()),
        [sort] if SORTS.contains(sort) => listing(format!("/{sort}")),
        ["search"] => listing("/search".to_owned()),
        ["r", name] if is_name(name) => listing(format!("/r/{name}")),
        ["r", name, "search"] if is_name(name) => listing(format!("/r/{name}/search")),
        ["r", name, sort] if is_name(name) && SORTS.contains(sort) => {
            listing(format!("/r/{name}/{sort}"))
        }
        ["r", name, "comments", id, rest @ ..] if is_name(name) && is_id(id) => {
            thread(format!("/r/{name}/comments/{id}"), rest)
        }
        ["comments", id, rest @ ..] if is_id(id) => thread(format!("/comments/{id}"), rest),
        ["user" | "u", name] if is_name(name) => listing(format!("/user/{name}")),
        ["user" | "u", name, tab] if is_name(name) && USER_TABS.contains(tab) => {
            listing(format!("/user/{name}/{tab}"))
        }
        _ => None,
    }
}

fn thread(base: String, rest: &[&str]) -> Option<(String, Shape)> {
    match rest {
        [] | [_] => Some((base, Shape::Thread)),
        [slug, comment] if is_id(comment) => {
            Some((format!("{base}/{slug}/{comment}"), Shape::Thread))
        }
        _ => None,
    }
}

fn is_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn is_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+'))
}

struct Writer {
    markdown: bool,
    out: String,
}

impl Writer {
    const fn new(markdown: bool) -> Self {
        Self {
            markdown,
            out: String::new(),
        }
    }

    fn strong(&self, text: &str) -> String {
        if self.markdown {
            format!("**{text}**")
        } else {
            text.to_owned()
        }
    }

    fn lines(&mut self, indent: usize, text: &str) {
        for line in text.trim().lines() {
            if !line.trim().is_empty() {
                self.out.extend(std::iter::repeat_n(' ', indent));
                self.out.push_str(line.trim_end());
            }
            self.out.push('\n');
        }
    }

    fn blank(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn heading(&mut self, text: &str) {
        self.blank();
        let heading = if self.markdown {
            format!("## {text}")
        } else {
            text.to_owned()
        };
        self.lines(0, &heading);
        self.blank();
    }

    fn listed(&mut self, number: usize, item: Listed) {
        let title = self.strong(&item.title);
        self.lines(0, &format!("{number}. {title}"));
        self.lines(NUMBERED_INDENT, &item.details);
        if let Some(permalink) = item.permalink {
            self.lines(NUMBERED_INDENT, &permalink);
        }
        if let Some(link) = item.link {
            self.lines(NUMBERED_INDENT, &link_line(&link));
        }
        self.lines(NUMBERED_INDENT, &excerpt(&item.text));
        self.blank();
    }

    fn finish(self) -> String {
        self.out.trim_end().to_owned()
    }
}

/// One numbered entry of a subreddit, user, or search listing.
struct Listed {
    title: String,
    details: String,
    permalink: Option<String>,
    link: Option<String>,
    text: String,
}

/// Joins the present parts, once each: a user feed names the user as both its
/// community and its author.
fn meta(parts: impl IntoIterator<Item = Option<String>>) -> String {
    let mut shown = parts
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    shown.dedup();
    shown.join(SEPARATOR)
}

fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_EXCERPT_CHARS {
        return flat;
    }
    let cut = flat.chars().take(MAX_EXCERPT_CHARS).collect::<String>();
    format!("{}…", cut.trim_end())
}

fn link_line(link: &str) -> String {
    format!("Link: {link}")
}

fn user(name: &str) -> Option<String> {
    let name = name.trim().trim_start_matches('/').trim_start_matches("u/");
    (!name.is_empty()).then(|| format!("u/{name}"))
}

struct Entry {
    id: String,
    title: String,
    author: Option<String>,
    subreddit: Option<String>,
    date: Option<String>,
    link: Option<String>,
    content: String,
}

struct Feed {
    title: Option<String>,
    entries: Vec<Entry>,
}

fn child<'a, 'input>(node: Node<'a, 'input>, name: &str) -> Option<Node<'a, 'input>> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
}

fn text_of(node: Node<'_, '_>) -> String {
    node.descendants()
        .filter(Node::is_text)
        .filter_map(|text| text.text())
        .collect()
}

fn child_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    child(node, name)
        .map(text_of)
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn feed(text: &str) -> Result<Feed, SiteRenderError> {
    let document = roxmltree::Document::parse(text)
        .map_err(|error| SiteRenderError::Malformed(RSS, error.to_string()))?;
    let root = document.root_element();
    if root.tag_name().name() != "feed" {
        return Err(SiteRenderError::Malformed(
            RSS,
            "not an Atom feed".to_owned(),
        ));
    }
    let entries = root
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "entry")
        .map(|entry| Entry {
            id: child_text(entry, "id").unwrap_or_default(),
            title: child_text(entry, "title").unwrap_or_default(),
            author: child(entry, "author")
                .and_then(|author| child_text(author, "name"))
                .and_then(|name| user(&name)),
            subreddit: child(entry, "category")
                .and_then(|category| category.attribute("label"))
                .map(str::to_owned),
            date: child_text(entry, "published")
                .or_else(|| child_text(entry, "updated"))
                .map(|value| feed_date(&value)),
            link: child(entry, "link")
                .and_then(|link| link.attribute("href"))
                .map(str::to_owned),
            content: child(entry, "content").map(text_of).unwrap_or_default(),
        })
        .collect();
    Ok(Feed {
        title: child_text(root, "title"),
        entries,
    })
}

fn feed_date(value: &str) -> String {
    DateTime::parse_from_rfc3339(value).map_or_else(
        |_| value.to_owned(),
        |date| date.with_timezone(&Utc).format(DATE_FORMAT).to_string(),
    )
}

/// The post or comment body a feed entry carries, without Reddit's
/// "submitted by … [link] [comments]" trailer, and the target of its `[link]`
/// when that points away from the entry itself.
fn entry_parts(entry: &Entry, markdown: bool, base_url: &str) -> (String, Option<String>) {
    let document = Document::from(entry.content.as_str());
    let body = document.select_single("div.md");
    let body = if body.exists() {
        html_fragment(&body.html(), markdown, base_url)
    } else {
        String::new()
    };
    let permalink = entry.link.as_deref().map(|link| link.trim_end_matches('/'));
    let link = document
        .select("a")
        .iter()
        .find(|anchor| anchor.text().trim() == LINK_LABEL)
        .and_then(|anchor| anchor.attr("href"))
        .map(|href| href.to_string())
        .filter(|href| Some(href.trim_end_matches('/')) != permalink);
    (body, link)
}

fn thread_rss(text: &str, context: &RenderContext) -> Result<SitePage, SiteRenderError> {
    let Feed { entries, .. } = feed(text)?;
    let (posts, comments): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .filter(|entry| entry.id.starts_with(POST_PREFIX) || entry.id.starts_with(COMMENT_PREFIX))
        .partition(|entry| entry.id.starts_with(POST_PREFIX));
    let post = posts.first().ok_or(SiteRenderError::Empty(RSS))?;
    let base_url = context.final_url.as_str();
    let mut writer = Writer::new(context.markdown);
    let (body, link) = entry_parts(post, context.markdown, base_url);
    writer.lines(
        0,
        &meta([
            post.subreddit.clone(),
            post.author.clone(),
            post.date.clone(),
        ]),
    );
    if let Some(link) = link {
        writer.lines(0, &link_line(&link));
    }
    writer.blank();
    writer.lines(0, &body);
    writer.heading(&format!("Comments ({})", comments.len()));
    for comment in &comments {
        let author = comment
            .author
            .as_deref()
            .map(|author| writer.strong(author));
        writer.lines(0, &format!("- {}", meta([author, comment.date.clone()])));
        writer.lines(
            LIST_INDENT,
            &entry_parts(comment, context.markdown, base_url).0,
        );
    }
    Ok(SitePage {
        title: Some(post.title.clone()).filter(|title| !title.is_empty()),
        body: writer.finish(),
        method: RSS_METHOD,
    })
}

fn listing_rss(text: &str, context: &RenderContext) -> Result<SitePage, SiteRenderError> {
    let Feed { title, entries } = feed(text)?;
    if entries.is_empty() {
        return Err(SiteRenderError::Empty(RSS));
    }
    let base_url = context.final_url.as_str();
    let mut writer = Writer::new(context.markdown);
    for (index, entry) in entries.into_iter().enumerate() {
        let (text, link) = entry_parts(&entry, false, base_url);
        let details = meta([entry.subreddit, entry.author, entry.date]);
        writer.listed(
            index + 1,
            Listed {
                title: entry.title,
                details,
                permalink: entry.link,
                link,
                text,
            },
        );
    }
    Ok(SitePage {
        title,
        body: writer.finish(),
        method: RSS_METHOD,
    })
}

#[derive(Default, Deserialize)]
struct Listing {
    #[serde(default)]
    data: ListingData,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ListingData {
    children: Vec<Thing>,
}

#[derive(Deserialize)]
struct Thing {
    kind: String,
    #[serde(default)]
    data: Value,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Post {
    title: String,
    subreddit_name_prefixed: String,
    author: String,
    score: Option<i64>,
    upvote_ratio: Option<f64>,
    num_comments: Option<u64>,
    link_flair_text: Option<String>,
    selftext: String,
    selftext_html: Option<String>,
    url: String,
    is_self: bool,
    permalink: String,
    created_utc: Option<f64>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Comment {
    author: String,
    score: Option<i64>,
    body: String,
    body_html: Option<String>,
    created_utc: Option<f64>,
    replies: Value,
    link_title: Option<String>,
    permalink: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct More {
    count: u64,
}

enum Item {
    Post(Box<Post>),
    Comment(Box<Comment>),
    More(More),
    Other,
}

impl Thing {
    fn item(self) -> Item {
        let item = match self.kind.as_str() {
            "t3" => serde_json::from_value(self.data).map(|post| Item::Post(Box::new(post))),
            "t1" => {
                serde_json::from_value(self.data).map(|comment| Item::Comment(Box::new(comment)))
            }
            "more" => serde_json::from_value(self.data).map(Item::More),
            _ => Ok(Item::Other),
        };
        item.unwrap_or(Item::Other)
    }
}

impl Comment {
    fn replies(&mut self) -> Vec<Thing> {
        serde_json::from_value::<Listing>(self.replies.take())
            .map(|listing| listing.data.children)
            .unwrap_or_default()
    }
}

fn timestamp(seconds: Option<f64>) -> Option<String> {
    let seconds = seconds.filter(|seconds| seconds.is_finite() && *seconds > 0.0)?;
    DateTime::from_timestamp(seconds as i64, 0).map(|date| date.format(DATE_FORMAT).to_string())
}

fn points(score: Option<i64>) -> Option<String> {
    score.map(|score| format!("{score} points"))
}

fn permalink(path: &str) -> Option<String> {
    (!path.is_empty()).then(|| format!("{ORIGIN}{path}"))
}

/// Reddit's Markdown source in Markdown mode, its rendered HTML as plain text
/// otherwise.
fn json_body(source: &str, html: Option<&str>, context: &RenderContext) -> String {
    match html {
        Some(html) if !context.markdown => html_fragment(html, false, context.final_url.as_str()),
        _ => source.trim().to_owned(),
    }
}

fn json_error(error: &serde_json::Error) -> SiteRenderError {
    SiteRenderError::Malformed(JSON, error.to_string())
}

fn thread_json(text: &str, context: &RenderContext) -> Result<SitePage, SiteRenderError> {
    let listings =
        serde_json::from_str::<Vec<Listing>>(text).map_err(|error| json_error(&error))?;
    let [posts, comments] = <[Listing; 2]>::try_from(listings).map_err(|_| {
        SiteRenderError::Malformed(JSON, "expected a post and its comments".to_owned())
    })?;
    let post = posts
        .data
        .children
        .into_iter()
        .find_map(|thing| match thing.item() {
            Item::Post(post) => Some(post),
            _ => None,
        })
        .ok_or(SiteRenderError::Empty(JSON))?;
    let mut writer = Writer::new(context.markdown);
    writer.lines(
        0,
        &meta([
            Some(post.subreddit_name_prefixed.clone()),
            user(&post.author),
            points(post.score),
            post.upvote_ratio
                .map(|ratio| format!("{}% upvoted", (ratio * 100.0).round())),
            post.num_comments.map(|count| format!("{count} comments")),
            post.link_flair_text.clone(),
            timestamp(post.created_utc),
        ]),
    );
    if !post.is_self && !post.url.is_empty() {
        writer.lines(0, &link_line(&post.url));
    }
    writer.blank();
    writer.lines(
        0,
        &json_body(&post.selftext, post.selftext_html.as_deref(), context),
    );
    writer.heading(&match post.num_comments {
        Some(count) => format!("Comments ({count})"),
        None => "Comments".to_owned(),
    });
    write_comments(&mut writer, comments.data.children, 0, context);
    Ok(SitePage {
        title: Some(post.title).filter(|title| !title.is_empty()),
        body: writer.finish(),
        method: JSON_METHOD,
    })
}

fn write_comments(writer: &mut Writer, things: Vec<Thing>, depth: usize, context: &RenderContext) {
    let indent = depth * LIST_INDENT;
    for thing in things {
        match thing.item() {
            Item::Comment(mut comment) => {
                let author = user(&comment.author).map(|author| writer.strong(&author));
                writer.lines(
                    indent,
                    &format!(
                        "- {}",
                        meta([
                            author,
                            points(comment.score),
                            timestamp(comment.created_utc)
                        ])
                    ),
                );
                writer.lines(
                    indent + LIST_INDENT,
                    &json_body(&comment.body, comment.body_html.as_deref(), context),
                );
                let replies = comment.replies();
                if replies.is_empty() {
                    continue;
                }
                if depth + 1 >= MAX_COMMENT_DEPTH {
                    writer.lines(
                        indent + LIST_INDENT,
                        &format!("- {} deeper replies not shown", replies.len()),
                    );
                } else {
                    write_comments(writer, replies, depth + 1, context);
                }
            }
            Item::More(more) if more.count > 0 => {
                writer.lines(indent, &format!("- {} more replies not loaded", more.count));
            }
            Item::Post(_) | Item::More(_) | Item::Other => {}
        }
    }
}

fn listing_json(text: &str, context: &RenderContext) -> Result<SitePage, SiteRenderError> {
    let listing = serde_json::from_str::<Listing>(text).map_err(|error| json_error(&error))?;
    let mut writer = Writer::new(context.markdown);
    let mut index = 0;
    for thing in listing.data.children {
        let listed = match thing.item() {
            Item::Post(post) => Listed {
                details: meta([
                    Some(post.subreddit_name_prefixed),
                    user(&post.author),
                    points(post.score),
                    post.num_comments.map(|count| format!("{count} comments")),
                    timestamp(post.created_utc),
                ]),
                title: post.title,
                permalink: permalink(&post.permalink),
                link: (!post.is_self && !post.url.is_empty()).then_some(post.url),
                text: post.selftext,
            },
            Item::Comment(comment) => Listed {
                title: format!(
                    "Comment on {}",
                    comment.link_title.as_deref().unwrap_or(UNNAMED_POST)
                ),
                details: meta([
                    user(&comment.author),
                    points(comment.score),
                    timestamp(comment.created_utc),
                ]),
                permalink: permalink(&comment.permalink),
                link: None,
                text: comment.body,
            },
            Item::More(_) | Item::Other => continue,
        };
        index += 1;
        writer.listed(index, listed);
    }
    if index == 0 {
        return Err(SiteRenderError::Empty(JSON));
    }
    Ok(SitePage {
        title: None,
        body: writer.finish(),
        method: JSON_METHOD,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const THREAD_RSS: &str = include_str!("../../../tests/fixtures/reddit/thread.rss");
    const LISTING_RSS: &str = include_str!("../../../tests/fixtures/reddit/listing.rss");
    const THREAD_JSON: &str = include_str!("../../../tests/fixtures/reddit/thread.json");
    const FEED_URL: &str = "https://www.reddit.com/r/rustexample/comments/abc123/.rss";

    fn attempt_urls(url: &str, format: WebfetchFormat) -> Option<Vec<String>> {
        let url = Url::parse(url).unwrap();
        Reddit.plan(&url, format).map(|plan| {
            plan.attempts
                .into_iter()
                .map(|attempt| attempt.url.to_string())
                .collect()
        })
    }

    fn context(markdown: bool) -> RenderContext {
        RenderContext {
            final_url: Url::parse(FEED_URL).unwrap(),
            markdown,
        }
    }

    #[test_case(
        "https://www.reddit.com/r/rust/comments/abc123/some_title/?utm_source=share",
        "https://www.reddit.com/r/rust/comments/abc123/.rss?limit=100",
        "https://www.reddit.com/r/rust/comments/abc123/.json?raw_json=1&limit=100";
        "thread_with_slug_drops_tracking_query"
    )]
    #[test_case(
        "https://old.reddit.com/r/rust/comments/abc123/",
        "https://www.reddit.com/r/rust/comments/abc123/.rss?limit=100",
        "https://www.reddit.com/r/rust/comments/abc123/.json?raw_json=1&limit=100";
        "old_reddit_thread"
    )]
    #[test_case(
        "https://redd.it/abc123",
        "https://www.reddit.com/comments/abc123/.rss?limit=100",
        "https://www.reddit.com/comments/abc123/.json?raw_json=1&limit=100";
        "short_link"
    )]
    #[test_case(
        "https://www.reddit.com/r/rust/comments/abc123/some_title/def456/",
        "https://www.reddit.com/r/rust/comments/abc123/some_title/def456/.rss?limit=100",
        "https://www.reddit.com/r/rust/comments/abc123/some_title/def456/.json?raw_json=1&limit=100";
        "comment_permalink"
    )]
    #[test_case(
        "https://www.reddit.com/r/rust/comments/abc123.json",
        "https://www.reddit.com/r/rust/comments/abc123/.rss?limit=100",
        "https://www.reddit.com/r/rust/comments/abc123/.json?raw_json=1&limit=100";
        "json_thread_url"
    )]
    #[test_case(
        "https://www.reddit.com/r/rust/top/?t=week&utm_name=x",
        "https://www.reddit.com/r/rust/top/.rss?t=week",
        "https://www.reddit.com/r/rust/top/.json?raw_json=1&t=week";
        "subreddit_sort_keeps_time_range"
    )]
    #[test_case(
        "https://www.reddit.com/r/rust",
        "https://www.reddit.com/r/rust/.rss",
        "https://www.reddit.com/r/rust/.json?raw_json=1";
        "subreddit"
    )]
    #[test_case(
        "https://www.reddit.com/u/example_user/",
        "https://www.reddit.com/user/example_user/.rss",
        "https://www.reddit.com/user/example_user/.json?raw_json=1";
        "user"
    )]
    #[test_case(
        "https://www.reddit.com/r/rust/search/?q=async+traits&restrict_sr=1",
        "https://www.reddit.com/r/rust/search/.rss?q=async+traits&restrict_sr=1",
        "https://www.reddit.com/r/rust/search/.json?raw_json=1&q=async+traits&restrict_sr=1";
        "subreddit_search"
    )]
    fn known_pages_are_read_from_the_feed_then_the_api(url: &str, rss: &str, json: &str) {
        assert_eq!(
            attempt_urls(url, WebfetchFormat::Markdown),
            Some(vec![rss.to_owned(), json.to_owned()])
        );
    }

    #[test_case("https://www.reddit.com/settings/profile", WebfetchFormat::Markdown; "unknown_reddit_path")]
    #[test_case("https://www.reddit.com/r/rust/s/AbCdEf", WebfetchFormat::Markdown; "share_link")]
    #[test_case("https://notreddit.com/r/rust", WebfetchFormat::Markdown; "lookalike_host")]
    #[test_case("https://example.com/r/rust/comments/abc123/", WebfetchFormat::Markdown; "other_host")]
    #[test_case("https://www.reddit.com/r/rust/comments/abc123/", WebfetchFormat::Html; "raw_html_requested")]
    fn other_requests_are_fetched_unchanged(url: &str, format: WebfetchFormat) {
        assert_eq!(attempt_urls(url, format), None);
    }

    #[test]
    fn the_feed_attempt_falls_back_only_on_a_refusal() {
        let url = Url::parse("https://www.reddit.com/r/rust/").unwrap();
        let plan = Reddit.plan(&url, WebfetchFormat::Markdown).unwrap();
        assert_eq!(plan.attempts[0].fallback_on, RSS_FALLBACK_ON);
        assert_eq!(plan.attempts[0].headers[http::header::ACCEPT], RSS_ACCEPT);
        assert_eq!(
            plan.attempts[0].headers[http::header::USER_AGENT],
            USER_AGENT
        );
        assert!(plan.attempts[1].fallback_on.is_empty());
        assert_eq!(plan.attempts[1].headers[http::header::ACCEPT], JSON_ACCEPT);
    }

    #[test]
    fn a_thread_feed_renders_the_post_and_its_comments_in_order() {
        let page = thread_rss(THREAD_RSS, &context(true)).unwrap();
        assert_eq!(
            page.title.as_deref(),
            Some("Which allocator do you ship with?")
        );
        assert_eq!(page.method, RSS_METHOD);
        assert!(page.body.starts_with(
            "r/rustexample · u/example_author · 2026-03-24 00:22 UTC\n\nWe switched our service to a **custom allocator** last month."
        ));
        assert!(!page.body.contains("submitted by"));
        assert!(!page.body.contains("Link:"));
        let first = page
            .body
            .find("- **u/first_reply** · 2026-03-24 02:08 UTC\n  The system allocator")
            .unwrap();
        let second = page.body.find("- **u/second_reply**").unwrap();
        assert!(page.body.contains("## Comments (2)"));
        assert!(first < second);
    }

    #[test]
    fn a_thread_feed_renders_plain_text() {
        let page = thread_rss(THREAD_RSS, &context(false)).unwrap();
        assert!(page.body.contains("custom allocator last month."));
        assert!(page.body.contains("\nComments (2)\n"));
        assert!(!page.body.contains("**"));
    }

    #[test]
    fn a_listing_feed_numbers_posts_and_names_external_links() {
        let page = listing_rss(LISTING_RSS, &context(true)).unwrap();
        assert_eq!(
            page.title.as_deref(),
            Some("newest submissions : rustexample")
        );
        assert!(page.body.starts_with(
            "1. **Release notes for the example toolkit**\n   r/rustexample · u/link_poster"
        ));
        assert!(
            page.body
                .contains("   Link: https://blog.example.test/release-notes")
        );
        assert!(
            page.body
                .contains("2. **Feedback wanted on a terminal client**")
        );
        assert!(page.body.contains("   I wrote a small terminal client"));
        assert_eq!(page.body.matches("Link:").count(), 1);
    }

    #[test]
    fn a_thread_response_nests_replies_and_counts_unloaded_ones() {
        let page = thread_json(THREAD_JSON, &context(true)).unwrap();
        assert_eq!(page.method, JSON_METHOD);
        assert!(page.body.starts_with(
            "r/rustexample · u/example_author · 42 points · 93% upvoted · 5 comments · Discussion · 2026-03-24 00:22 UTC"
        ));
        assert!(page.body.contains("## Comments (5)"));
        assert!(page.body.contains("- **u/first_reply** · 17 points · "));
        assert!(page.body.contains("\n  - **u/nested_reply** · 4 points · "));
        assert!(page.body.contains("\n    Agreed, measure first."));
        assert!(page.body.contains("\n  - 2 more replies not loaded"));
        assert!(page.body.contains("- **u/second_reply** · 2026-03-25"));
    }

    #[test]
    fn replies_past_the_depth_cap_are_counted_not_rendered() {
        let mut replies = json!("");
        for depth in (0..=MAX_COMMENT_DEPTH).rev() {
            replies = json!({"kind": "Listing", "data": {"children": [
                {"kind": "t1", "data": {"author": format!("depth_{depth}"), "body": "reply", "replies": replies}}
            ]}});
        }
        let thread = json!([
            {"kind": "Listing", "data": {"children": [{"kind": "t3", "data": {"title": "Deep", "is_self": true}}]}},
            replies,
        ]);
        let page = thread_json(&thread.to_string(), &context(true)).unwrap();
        let last_shown = format!("u/depth_{}", MAX_COMMENT_DEPTH - 1);
        let hidden = format!("u/depth_{MAX_COMMENT_DEPTH}");
        assert!(page.body.contains(&last_shown));
        assert!(!page.body.contains(&hidden));
        assert!(page.body.contains("- 1 deeper replies not shown"));
    }

    #[test_case("<html><body>blocked</body></html>", thread_rss; "html_instead_of_a_feed")]
    #[test_case("<feed", thread_rss; "truncated_xml")]
    #[test_case("{\"kind\": \"Listing\"}", thread_json; "listing_instead_of_a_thread")]
    #[test_case("[{", thread_json; "truncated_json")]
    fn malformed_responses_are_render_errors(body: &str, render: Render) {
        assert!(matches!(
            render(body, &context(true)),
            Err(SiteRenderError::Malformed(..))
        ));
    }

    #[test]
    fn a_user_named_as_community_and_author_is_shown_once() {
        let user = Some("u/example_user".to_owned());
        assert_eq!(meta([user.clone(), user, None]), "u/example_user");
    }

    #[test]
    fn an_empty_listing_is_a_render_error() {
        let empty = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><title>t</title></feed>"#;
        assert!(matches!(
            listing_rss(empty, &context(true)),
            Err(SiteRenderError::Empty(_))
        ));
    }
}
