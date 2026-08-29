//! What a link to this server looks like when it is pasted somewhere else.
//!
//! Chat clients, search engines and social sites do not run JavaScript. They
//! fetch the URL once, read the `<head>`, and render whatever they find there.
//! A single-page app serves the same empty shell for every route, so every link
//! to this hub arrived as a bare domain name with no title and no picture.
//!
//! This fills that in: the shell is rewritten per route on the way out, an
//! oEmbed document is offered for the clients that ask for one, and a social
//! card is drawn as a real image.
//!
//! # Visibility
//!
//! Everything here is resolved as an anonymous viewer, never as the requester.
//! A crawler has no session, so that is already what it is — but the important
//! half is the other direction: this must never describe something the person
//! who eventually clicks the link would not be allowed to see. A chat client
//! shows an unfurled link to a whole channel, so leaking a private repository's
//! name or description into one would republish it to everybody in the room.
//! A page that is not publicly readable gets the generic site metadata, which
//! is the same answer it gives for a path that does not exist.

use crate::auth::Viewer;
use crate::state::AppState;
use fkit_core::Hash;

/// A page, described for something that will not run our JavaScript.
pub struct Meta {
    pub title: String,
    pub description: String,
    /// Absolute, because a crawler resolves nothing relative to the page.
    pub url: String,
    /// Absolute URL of the social card, when this page has one.
    pub image: Option<String>,
    pub og_type: &'static str,
    /// What the card should draw. `None` means fall back to the site card.
    pub card: Option<Card>,
    /// This page's colour. Discord paints the embed's edge with it, so the
    /// bar beside the message matches the bar on the card inside it.
    pub tint: String,
}

/// The content of a social card.
///
/// One shape for every kind of page rather than a variant each: they differ in
/// what goes in the slots, not in how the slots are arranged, and a card that
/// is laid out one way is a card people recognise at a glance.
#[derive(Clone, Default)]
pub struct Card {
    /// Small line above the title — usually the owner, with its slash.
    pub eyebrow: String,
    /// The one big thing.
    pub title: String,
    /// Free text under the title. Wrapped, and truncated if it will not fit.
    pub body: String,
    /// `(value, label)` pairs along the bottom.
    pub facts: Vec<(String, String)>,
    /// Dim monospace line at the very bottom. A hash, where there is one:
    /// in a content-addressed store that *is* the identity of what is shown.
    pub footer: String,
    /// Short word in the top right — "public", "open", "merged".
    pub badge: Option<Badge>,
    /// The colour of whatever this card is about. Empty falls back to the
    /// brand accent.
    pub tint: String,
}

/// A word in the top right, and the colour that word earns.
#[derive(Clone)]
pub struct Badge {
    pub text: String,
    /// Stroke and text colour. A state worth reacting to gets the accent; a
    /// label that is merely descriptive stays grey, so colour keeps meaning
    /// something on a card that is read at a glance.
    pub tone: &'static str,
}

impl Badge {
    fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), tone: MUTED }
    }

    /// The state of an issue or a merge request.
    fn state(state: &str) -> Self {
        let tone = match state {
            "open" => ACCENT,
            "merged" => TOK_KEYWORD,
            _ => FAINT,
        };
        Self { text: state.to_string(), tone }
    }
}

// ---- describing a route --------------------------------------------------

/// Work out what a path is, or `None` if it is not something we describe.
///
/// `path` is the request path, already percent-decoded by the router.
pub async fn describe(state: &AppState, path: &str, base: &str) -> Option<Meta> {
    let seg: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let url = format!("{base}{path}");

    match seg.as_slice() {
        [] => Some(site_meta(base)),
        [owner] => user_meta(state, owner, &url, base).await,
        [owner, repo] => repo_meta(state, owner, repo, &url, base).await,
        [owner, repo, "issues", n] => {
            thread_meta(state, owner, repo, n.parse().ok()?, Kind::Issue, &url, base).await
        }
        [owner, repo, "merges", n, ..] => {
            thread_meta(state, owner, repo, n.parse().ok()?, Kind::Merge, &url, base).await
        }
        [owner, repo, "commit", hash] => {
            commit_meta(state, owner, repo, hash, &url, base).await
        }
        [owner, repo, "blob", _ref_and_path @ ..] => {
            // The ref and the path are not separable without knowing the
            // branch names, and the card does not need them: the file's own
            // name is the last segment, and that is what a reader recognises.
            let file = seg.last().copied().unwrap_or_default();
            blob_meta(state, owner, repo, file, &url, base).await
        }
        // Every other route under a repository — tree, history, tags, settings
        // — describes the repository. Better than nothing, and never wrong.
        [owner, repo, ..] => repo_meta(state, owner, repo, &url, base).await,
    }
}

enum Kind {
    Issue,
    Merge,
}

/// The whole site, for the root and for anything not publicly readable.
pub fn site_meta(base: &str) -> Meta {
    Meta {
        title: "fkit".into(),
        description: "Content-addressed version control. Chunk-level deduplication, \
                      no repacking, and large files that do not ruin the repository."
            .into(),
        url: base.to_string(),
        image: Some(format!("{base}/og/site.png")),
        og_type: "website",
        tint: ACCENT.to_string(),
        card: Some(Card {
            tint: ACCENT.to_string(),
            eyebrow: String::new(),
            title: "fkit".into(),
            body: "Content-addressed version control.".into(),
            facts: vec![],
            footer: base.trim_start_matches("https://").trim_start_matches("http://").into(),
            badge: None,
        }),
    }
}

async fn repo_meta(
    state: &AppState,
    owner: &str,
    name: &str,
    url: &str,
    base: &str,
) -> Option<Meta> {
    let (repo, access, owner_name) =
        crate::routes::load_repo(state, &Viewer::anonymous(), owner, name).await.ok()?;
    if !access.can_read() {
        return None;
    }
    let slug = format!("{owner_name}/{}", repo.name);

    let description = repo
        .description
        .clone()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| format!("{owner_name}/{} on fkit", repo.name));

    // Cheap counts only. A crawler is not worth walking a store for.
    let branches: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM refs WHERE repo_id = $1 AND name NOT LIKE 'tags/%'",
    )
    .bind(repo.id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    let tip: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT target FROM refs WHERE repo_id = $1 AND name = $2")
            .bind(repo.id)
            .bind(&repo.default_branch)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();

    let tags: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM refs WHERE repo_id = $1 AND name LIKE 'tags/%'",
    )
    .bind(repo.id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    let forks: i64 = sqlx::query_scalar("SELECT count(*) FROM repos WHERE forked_from = $1")
        .bind(repo.id)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0);

    let issues: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM issues WHERE repo_id = $1 AND state = 'open'",
    )
    .bind(repo.id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    // Shown even at zero, the way a forge's own header shows them: a reader
    // scanning several cards wants the same four numbers in the same places,
    // and a row that changes shape per repository is harder to read than one
    // that occasionally says nought.
    let facts = vec![
        (plural(branches, "branch", "branches"), String::new()),
        (plural(tags, "tag", "tags"), String::new()),
        (plural(forks, "fork", "forks"), String::new()),
        (plural(issues, "issue", "issues"), String::new()),
    ];

    Some(Meta {
        title: format!("{owner_name}/{}", repo.name),
        description: description.clone(),
        url: url.to_string(),
        image: Some(format!("{base}/og/{owner_name}/{}.png", repo.name)),
        og_type: "object",
        tint: tint(&slug),
        card: Some(Card {
            tint: tint(&slug),
            eyebrow: format!("{owner_name} /"),
            title: repo.name.clone(),
            body: description,
            facts,
            footer: tip.map(hex).unwrap_or_default(),
            badge: (repo.visibility == "public").then(|| Badge::plain("public")),
        }),
    })
}

async fn user_meta(state: &AppState, name: &str, url: &str, base: &str) -> Option<Meta> {
    let row: Option<(String, Option<String>, i64)> = sqlx::query_as(
        "SELECT u.username, u.display_name,
                (SELECT count(*) FROM repos r
                  WHERE r.owner_id = u.id AND r.visibility = 'public')
           FROM users u WHERE u.username = $1",
    )
    .bind(name.to_ascii_lowercase())
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let (username, display, repos) = row?;
    // A display name that merely repeats the username says nothing.
    let display = display
        .filter(|d| !d.trim().is_empty() && !d.eq_ignore_ascii_case(&username));
    let description = match &display {
        Some(d) => format!("{d} on fkit"),
        None => format!("{username} on fkit"),
    };

    Some(Meta {
        title: username.clone(),
        description: description.clone(),
        url: url.to_string(),
        image: Some(format!("{base}/og/{username}.png")),
        og_type: "profile",
        tint: tint(&username),
        card: Some(Card {
            tint: tint(&username),
            eyebrow: String::new(),
            title: username,
            body: display.unwrap_or_default(),
            facts: vec![(plural(repos, "repository", "repositories"), String::new())],
            footer: String::new(),
            badge: None,
        }),
    })
}

async fn thread_meta(
    state: &AppState,
    owner: &str,
    name: &str,
    number: i32,
    kind: Kind,
    url: &str,
    base: &str,
) -> Option<Meta> {
    let (repo, access, owner_name) =
        crate::routes::load_repo(state, &Viewer::anonymous(), owner, name).await.ok()?;
    if !access.can_read() {
        return None;
    }

    // Written out per table rather than interpolated. The two differ in more
    // than their name — an issue's long field is `body`, a merge request's is
    // `description` — and sqlx rightly refuses a query built by formatting.
    let row: Option<(String, Option<String>, String, Option<String>)> = match kind {
        Kind::Issue => sqlx::query_as(
            "SELECT t.title, t.body, t.state, u.username FROM issues t
               LEFT JOIN users u ON u.id = t.author_id
              WHERE t.repo_id = $1 AND t.number = $2",
        ),
        Kind::Merge => sqlx::query_as(
            "SELECT t.title, t.description, t.state, u.username FROM merge_requests t
               LEFT JOIN users u ON u.id = t.author_id
              WHERE t.repo_id = $1 AND t.number = $2",
        ),
    }
    .bind(repo.id)
    .bind(number)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let (title, body, thread_state, author) = row?;

    let slug = format!("{owner_name}/{}", repo.name);
    let (label, route) = match kind {
        Kind::Issue => ("issue", "issues"),
        Kind::Merge => ("merge request", "merges"),
    };
    let body = body.unwrap_or_default();
    let description = if body.trim().is_empty() {
        format!("{label} #{number} in {owner_name}/{}", repo.name)
    } else {
        body.clone()
    };

    Some(Meta {
        title: format!("{title} · {label} #{number} · {owner_name}/{}", repo.name),
        description: description.clone(),
        url: url.to_string(),
        image: Some(format!(
            "{base}/og/{owner_name}/{}/{route}/{number}.png",
            repo.name
        )),
        og_type: "article",
        tint: tint(&slug),
        card: Some(Card {
            tint: tint(&slug),
            eyebrow: format!("{owner_name} / {}  ·  #{number}", repo.name),
            title,
            body,
            // Who opened it is the fact a reader wants next, and it keeps the
            // card from ending on an empty rule.
            facts: author
                .map(|a| vec![(format!("opened by {a}"), String::new())])
                .unwrap_or_default(),
            footer: String::new(),
            badge: Some(Badge::state(&thread_state)),
        }),
    })
}

async fn commit_meta(
    state: &AppState,
    owner: &str,
    name: &str,
    hash: &str,
    url: &str,
    base: &str,
) -> Option<Meta> {
    let (repo, access, owner_name) =
        crate::routes::load_repo(state, &Viewer::anonymous(), owner, name).await.ok()?;
    if !access.can_read() {
        return None;
    }

    let slug = format!("{owner_name}/{}", repo.name);
    let store = state.store_for_network(repo.network_id).ok()?;
    let id = Hash::from_hex(hash).or_else(|| store.resolve_prefix(hash).ok())?;
    let fkit_core::Object::Commit(c) = store.get(id).ok()? else {
        return None;
    };

    let summary = c.message.lines().next().unwrap_or_default().to_string();
    let hex = id.to_hex();

    // The account behind it, where there is one. Same rule as the commit list:
    // the author string is what the commit claims, the account is what is known.
    let pushed: Option<String> = sqlx::query_scalar(
        "SELECT u.username FROM commit_authors ca
           JOIN users u ON u.id = ca.user_id
          WHERE ca.commit_hash = $1",
    )
    .bind(id.0.to_vec())
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    // "Name <email>" -> "Name". The same reduction the commit list makes.
    let who = match c.author.find('<') {
        Some(i) => c.author[..i].trim().to_string(),
        None => c.author.trim().to_string(),
    };
    let byline = match &pushed {
        Some(acct) if !acct.eq_ignore_ascii_case(&who) => format!("{who}, pushed by {acct}"),
        Some(acct) => acct.clone(),
        None => who,
    };

    Some(Meta {
        title: format!("{summary} · {owner_name}/{}@{}", repo.name, &hex[..10]),
        description: format!("{byline} · {}", c.message.trim()),
        url: url.to_string(),
        image: Some(format!("{base}/og/{owner_name}/{}/commit/{hex}.png", repo.name)),
        og_type: "article",
        tint: tint(&slug),
        card: Some(Card {
            tint: tint(&slug),
            eyebrow: format!("{owner_name} / {}", repo.name),
            title: summary,
            body: byline,
            facts: vec![],
            footer: hex,
            badge: Some(Badge::plain("commit")),
        }),
    })
}

async fn blob_meta(
    state: &AppState,
    owner: &str,
    name: &str,
    file: &str,
    url: &str,
    base: &str,
) -> Option<Meta> {
    let (repo, access, owner_name) =
        crate::routes::load_repo(state, &Viewer::anonymous(), owner, name).await.ok()?;
    if !access.can_read() {
        return None;
    }
    let slug = format!("{owner_name}/{}", repo.name);
    Some(Meta {
        title: format!("{file} · {owner_name}/{}", repo.name),
        description: repo
            .description
            .clone()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| format!("{file} in {owner_name}/{}", repo.name)),
        url: url.to_string(),
        image: Some(format!("{base}/og/{owner_name}/{}.png", repo.name)),
        og_type: "object",
        tint: tint(&slug),
        card: Some(Card {
            tint: tint(&slug),
            eyebrow: format!("{owner_name} / {}", repo.name),
            title: file.to_string(),
            body: repo.description.unwrap_or_default(),
            facts: vec![],
            footer: String::new(),
            badge: Some(Badge::plain("file")),
        }),
    })
}

fn hex(b: Vec<u8>) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ---- putting it into the page --------------------------------------------

/// The head of a page there is nothing public to say about.
///
/// Deliberately not a generic card. GitHub answers a private repository with
/// its own site metadata, so the link still unfurls — with a title, a
/// description and a logo — and a dead link is presented to the channel
/// looking exactly like a live one. Emitting nothing is the more honest
/// answer: no preview appears at all, which is what "you cannot see this"
/// should look like.
///
/// `noindex` for the same reason. A page whose contents we decline to describe
/// is not a page a search engine should be keeping.
pub fn inject_blank(html: &str) -> String {
    insert_into_head(html, "<meta name=\"robots\" content=\"noindex\">\n")
}

/// Rewrite the SPA shell's `<head>` for this route.
///
/// Only inserts; nothing existing is removed, so the app boots exactly as it
/// did. The `<title>` is replaced because a crawler reads the first one and a
/// duplicate would be ignored.
pub fn inject(html: &str, meta: &Meta, base: &str, site: &str) -> String {
    let t = esc(&meta.title);
    let d = esc(&truncate(&meta.description, 300));
    let u = esc(&meta.url);

    let mut tags = String::with_capacity(1024);
    tags.push_str(&format!(
        "<meta property=\"og:site_name\" content=\"{}\">\n",
        esc(site)
    ));
    tags.push_str(&format!("<meta property=\"og:title\" content=\"{t}\">\n"));
    tags.push_str(&format!("<meta property=\"og:description\" content=\"{d}\">\n"));
    tags.push_str(&format!("<meta property=\"og:url\" content=\"{u}\">\n"));
    tags.push_str(&format!("<meta property=\"og:type\" content=\"{}\">\n", meta.og_type));
    tags.push_str(&format!("<meta name=\"description\" content=\"{d}\">\n"));

    if let Some(img) = &meta.image {
        let i = esc(img);
        tags.push_str(&format!("<meta property=\"og:image\" content=\"{i}\">\n"));
        tags.push_str("<meta property=\"og:image:width\" content=\"1200\">\n");
        tags.push_str("<meta property=\"og:image:height\" content=\"630\">\n");
        tags.push_str(&format!("<meta property=\"og:image:alt\" content=\"{t}\">\n"));
        tags.push_str("<meta name=\"twitter:card\" content=\"summary_large_image\">\n");
        tags.push_str(&format!("<meta name=\"twitter:image\" content=\"{i}\">\n"));
    } else {
        tags.push_str("<meta name=\"twitter:card\" content=\"summary\">\n");
    }
    tags.push_str(&format!("<meta name=\"twitter:title\" content=\"{t}\">\n"));
    tags.push_str(&format!("<meta name=\"twitter:description\" content=\"{d}\">\n"));

    // Discord reads this to draw the small provider line above the embed, and
    // it is the only way to get a coloured accent bar on one.
    let oembed = esc(&format!(
        "{base}/oembed?url={}",
        urlencode(&meta.url)
    ));
    tags.push_str(&format!(
        "<link rel=\"alternate\" type=\"application/json+oembed\" href=\"{oembed}\" title=\"{t}\">\n"
    ));
    // Discord paints the embed's left edge with this, so a repository's bar in
    // the channel is the same colour as the bar on the card inside it.
    let tint = if meta.tint.is_empty() { ACCENT } else { &meta.tint };
    tags.push_str(&format!("<meta name=\"theme-color\" content=\"{}\">\n", esc(tint)));

    insert_into_head(&replace_title(html, &t), &tags)
}

fn insert_into_head(html: &str, tags: &str) -> String {
    match html.find("</head>") {
        Some(i) => {
            let mut out = String::with_capacity(html.len() + tags.len());
            out.push_str(&html[..i]);
            out.push_str(tags);
            out.push_str(&html[i..]);
            out
        }
        None => html.to_string(),
    }
}

fn replace_title(html: &str, title: &str) -> String {
    let Some(open) = html.find("<title>") else { return html.to_string() };
    let Some(close) = html[open..].find("</title>").map(|i| i + open) else {
        return html.to_string();
    };
    let mut out = String::with_capacity(html.len() + title.len());
    out.push_str(&html[..open + "<title>".len()]);
    out.push_str(title);
    out.push_str(&html[close..]);
    out
}

/// Escape for an HTML attribute. Everything here is user content.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            // A newline inside an attribute is legal but wrecks the preview,
            // and a description is often a paragraph.
            '\n' | '\r' | '\t' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Cut to a character count, on a word boundary where one is close enough.
fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max - 1).collect();
    let cut = match cut.rfind(' ') {
        Some(i) if i > max / 2 => &cut[..i],
        _ => cut.as_str(),
    };
    format!("{}…", cut.trim_end())
}

// ---- the card ------------------------------------------------------------

const W: u32 = 1200;
const H: u32 = 630;
const BG: &str = "#090d0d";
const TEXT: &str = "#dde5e3";
const MUTED: &str = "#7d908c";
const FAINT: &str = "#556864";
const ACCENT: &str = "#4fb3a6";
const BORDER: &str = "#1d2726";
const TOK_KEYWORD: &str = "#c98bd9";

/// Liberation Mono, vendored so the card looks the same wherever this runs.
/// A server with no fonts installed is the normal case in a container.
const FONT_REGULAR: &[u8] = include_bytes!("../assets/fonts/LiberationMono-Regular.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../assets/fonts/LiberationMono-Bold.ttf");
const FAMILY: &str = "Liberation Mono";

// ---- a colour per thing --------------------------------------------------

/// A stable colour for a name.
///
/// FNV-1a over the lowercased name, which gives a well-spread hue for inputs
/// that differ by one character — `fkit` and `fkit2` land nowhere near each
/// other, which is the whole point of colouring a repository at all.
///
/// Only the hue varies. Lightness and chroma are fixed at the brand accent's,
/// in Oklch rather than HSL: equal HSL lightness is not equal *perceived*
/// lightness, so a hue sweep at fixed HSL gives glaring yellows and murky
/// blues. In Oklch every hue comes out with the same visual weight, so no
/// repository ends up with a bar that shouts and none with one that vanishes.
pub fn tint(name: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.to_ascii_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    oklch_to_hex(TINT_L, TINT_C, (h % 360) as f32)
}

/// Lightness and chroma every tint is generated at.
///
/// Lightness is the brand accent's, so a tint sits on the dark card with the
/// same weight the accent does. Chroma is higher than the accent's: at the
/// brand's own 0.09 a thirty-degree hue difference is barely visible and half
/// the repositories come out the same dusty sage. Above about 0.16 the cyans
/// and blues clip out of sRGB and flatten into bands, so this is the point
/// where hues separate cleanly and none of them clip.
const TINT_L: f32 = 0.70;
const TINT_C: f32 = 0.14;

/// Oklch -> sRGB hex, clamped into gamut.
fn oklch_to_hex(l: f32, c: f32, hue_deg: f32) -> String {
    let h = hue_deg.to_radians();
    let (a, b) = (c * h.cos(), c * h.sin());

    // Oklab -> LMS -> linear sRGB, per Björn Ottosson's definition.
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (lc, mc, sc) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

    let r = 4.076_741_7 * lc - 3.307_711_6 * mc + 0.230_969_94 * sc;
    let g = -1.268_438 * lc + 2.609_757_4 * mc - 0.341_319_38 * sc;
    let bl = -0.004_196_086 * lc - 0.703_418_6 * mc + 1.707_614_7 * sc;

    format!("#{:02x}{:02x}{:02x}", channel(r), channel(g), channel(bl))
}

/// Linear light to an 8-bit sRGB channel.
fn channel(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let srgb = if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (srgb * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Monospace advance width as a fraction of the font size.
///
/// True for this face, and the reason the card uses a monospace font for
/// everything: wrapping and fitting become arithmetic instead of a layout pass.
const ADVANCE: f32 = 0.6;

fn fits(text: &str, size: f32, width: f32) -> bool {
    text.chars().count() as f32 * size * ADVANCE <= width
}

/// Largest size from `sizes` at which the text fits, or the smallest given.
fn fit_size(text: &str, width: f32, sizes: &[f32]) -> f32 {
    for &s in sizes {
        if fits(text, s, width) {
            return s;
        }
    }
    *sizes.last().unwrap_or(&12.0)
}

/// Greedy wrap by character count, which is exact for a monospace face.
fn wrap(text: &str, size: f32, width: f32, max_lines: usize) -> Vec<String> {
    let per_line = (width / (size * ADVANCE)).floor().max(1.0) as usize;
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();

    for word in text.split_whitespace() {
        let candidate = if cur.is_empty() { word.len() } else { cur.len() + 1 + word.len() };
        if candidate <= per_line {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
            continue;
        }
        if !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
            if lines.len() == max_lines {
                break;
            }
        }
        // A single word longer than the line gets cut rather than overflowing.
        if word.len() > per_line {
            cur = word.chars().take(per_line).collect();
        } else {
            cur = word.to_string();
        }
    }
    if lines.len() < max_lines && !cur.is_empty() {
        lines.push(cur);
    }

    // Mark the truncation so a clipped sentence does not read as a complete one.
    if lines.len() == max_lines {
        let used: usize = lines.iter().map(|l| l.split_whitespace().count()).sum();
        if used < text.split_whitespace().count() {
            let last = lines.last_mut().unwrap();
            while last.chars().count() > per_line.saturating_sub(1) {
                last.pop();
            }
            last.push('…');
        }
    }
    lines
}

/// Escape for XML text content. The card is built as SVG from user strings.
fn xesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Control characters are not valid XML at all.
            c if (c as u32) < 0x20 => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Build the card as SVG.
pub fn card_svg(card: &Card) -> String {
    const PAD: f32 = 72.0;
    let inner = W as f32 - PAD * 2.0;
    let mut s = String::with_capacity(4096);

    s.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">"#
    ));
    s.push_str(&format!(r#"<rect width="{W}" height="{H}" fill="{BG}"/>"#));

    // The one piece of colour on the card, and it belongs to whatever the card
    // is about rather than to the site — two repositories side by side in a
    // channel are told apart by it before either title is read.
    let tint = if card.tint.is_empty() { ACCENT } else { card.tint.as_str() };
    s.push_str(&format!(r#"<rect x="0" y="0" width="10" height="{H}" fill="{tint}"/>"#));

    // -- wordmark, top left --------------------------------------------------
    s.push_str(&format!(
        r#"<text x="{PAD}" y="96" font-family="{FAMILY}" font-size="30" font-weight="bold" fill="{tint}">f<tspan fill="{MUTED}">kit</tspan></text>"#
    ));

    // -- badge, top right ----------------------------------------------------
    if let Some(badge) = &card.badge {
        let text = xesc(&badge.text);
        let w = text.chars().count() as f32 * 17.0 * ADVANCE + 28.0;
        let x = W as f32 - PAD - w;
        s.push_str(&format!(
            r#"<rect x="{x}" y="70" width="{w}" height="34" rx="3" fill="none" stroke="{}" stroke-opacity="0.55"/>"#,
            badge.tone
        ));
        s.push_str(&format!(
            r#"<text x="{}" y="93" font-family="{FAMILY}" font-size="17" fill="{}">{text}</text>"#,
            x + 14.0,
            badge.tone
        ));
    }

    // -- the middle block ----------------------------------------------------
    //
    // Laid out from a baseline of zero and then translated, so it can be
    // centred in whatever room is left between the header and the rule. Fixed
    // offsets left a card with a one-line description sitting high with a band
    // of dead space beneath it, and a three-line one nearly touching the rule.
    const TOP: f32 = 118.0;
    let rule = H as f32 - 132.0;

    let mut block = String::new();
    let mut y = 0.0f32;
    let mut first_size = 0.0f32;

    if !card.eyebrow.is_empty() {
        let size = fit_size(&card.eyebrow, inner, &[32.0, 28.0, 24.0, 20.0]);
        first_size = size;
        block.push_str(&format!(
            r#"<text x="{PAD}" y="{y}" font-family="{FAMILY}" font-size="{size}" fill="{MUTED}">{}</text>"#,
            xesc(&card.eyebrow)
        ));
        y += 74.0;
    }

    let size = fit_size(&card.title, inner, &[86.0, 72.0, 60.0, 48.0, 38.0, 30.0]);
    if first_size == 0.0 {
        first_size = size;
    }
    let title = if fits(&card.title, 30.0, inner) {
        card.title.clone()
    } else {
        // Even at the smallest size it does not fit; cut rather than overflow.
        let n = (inner / (30.0 * ADVANCE)) as usize;
        format!("{}…", card.title.chars().take(n.saturating_sub(1)).collect::<String>())
    };
    block.push_str(&format!(
        r#"<text x="{PAD}" y="{y}" font-family="{FAMILY}" font-size="{size}" font-weight="bold" fill="{TEXT}">{}</text>"#,
        xesc(&title)
    ));
    let mut last_size = size;

    if !card.body.trim().is_empty() {
        y += 62.0;
        let lines = wrap(card.body.trim(), 28.0, inner, 3);
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                y += 40.0;
            }
            block.push_str(&format!(
                r#"<text x="{PAD}" y="{y}" font-family="{FAMILY}" font-size="28" fill="{MUTED}">{}</text>"#,
                xesc(line)
            ));
        }
        last_size = 28.0;
    }

    // Visual extent, not baseline extent: text sits above its baseline by
    // roughly its cap height and hangs below by its descender.
    let top_of_block = first_size * 0.75;
    let height = top_of_block + y + last_size * 0.22;
    let dy = TOP + ((rule - TOP - height) / 2.0).max(0.0) + top_of_block;
    s.push_str(&format!(r#"<g transform="translate(0,{dy})">{block}</g>"#));

    // -- footer --------------------------------------------------------------
    // Drawn only when there is something below it. A rule with nothing under it
    // reads as a card that failed to finish loading.
    if card.facts.is_empty() && card.footer.is_empty() {
        s.push_str("</svg>");
        return s;
    }
    s.push_str(&format!(
        r#"<rect x="{PAD}" y="{rule}" width="{inner}" height="1" fill="{BORDER}"/>"#
    ));

    // Stats get the full width on their own line, and the hash sits under them
    // rather than beside them. Sharing one line meant a 64-character hash left
    // room for about two counts before either had to be truncated.
    if !card.facts.is_empty() {
        let text = card
            .facts
            .iter()
            .map(|(v, l)| if l.is_empty() { v.clone() } else { format!("{v} {l}") })
            .collect::<Vec<_>>()
            .join("  ·  ");
        let size = fit_size(&text, inner, &[24.0, 21.0, 18.0]);
        s.push_str(&format!(
            r#"<text x="{PAD}" y="{}" font-family="{FAMILY}" font-size="{size}" fill="{MUTED}">{}</text>"#,
            rule + 40.0,
            xesc(&text)
        ));
    }

    if !card.footer.is_empty() {
        let size = fit_size(&card.footer, inner, &[19.0, 17.0, 15.0]);
        s.push_str(&format!(
            r#"<text x="{PAD}" y="{}" font-family="{FAMILY}" font-size="{size}" fill="{FAINT}">{}</text>"#,
            rule + 78.0,
            xesc(&card.footer)
        ));
    }

    s.push_str("</svg>");
    s
}

/// Rasterise a card. The vendored font is the only one consulted.
pub fn render_png(card: &Card) -> Option<Vec<u8>> {
    use resvg::usvg;

    // Building the database costs a few milliseconds, so it is done once.
    static FONTS: std::sync::OnceLock<std::sync::Arc<usvg::fontdb::Database>> =
        std::sync::OnceLock::new();
    let fontdb = FONTS.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_font_data(FONT_REGULAR.to_vec());
        db.load_font_data(FONT_BOLD.to_vec());
        db.set_monospace_family(FAMILY);
        std::sync::Arc::new(db)
    });

    let opt = usvg::Options {
        fontdb: fontdb.clone(),
        font_family: FAMILY.to_string(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(&card_svg(card), &opt).ok()?;
    let mut pix = resvg::tiny_skia::Pixmap::new(W, H)?;
    resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pix.as_mut());
    pix.encode_png().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_escaping_closes_no_tags() {
        let evil = r#"</title><script>alert(1)</script>"#;
        let out = esc(evil);
        assert!(!out.contains('<'), "{out}");
        assert!(!out.contains('>'), "{out}");
    }

    #[test]
    fn a_quote_in_a_description_cannot_end_the_attribute() {
        let meta = Meta {
            title: r#"a" onload="x"#.into(),
            description: r#"b" onload="y"#.into(),
            url: "https://example.test/a".into(),
            image: None,
            og_type: "object",
            card: None,
            tint: String::new(),
        };
        let html = inject("<head><title>fkit</title></head>", &meta, "https://example.test", "fkit");
        assert!(!html.contains(r#"onload="x"#), "{html}");
        assert!(!html.contains(r#"onload="y"#), "{html}");
        assert!(html.contains("&quot;"), "{html}");
    }

    #[test]
    fn svg_text_is_escaped() {
        let card = Card {
            title: "</text><script>x</script>".into(),
            body: "a & b < c".into(),
            ..Default::default()
        };
        let svg = card_svg(&card);
        assert!(!svg.contains("<script>"), "{svg}");
        assert!(svg.contains("&amp;"), "{svg}");
    }

    #[test]
    fn the_title_is_replaced_not_appended() {
        let meta = Meta {
            title: "owner/repo".into(),
            description: String::new(),
            url: "https://e.test/owner/repo".into(),
            image: None,
            og_type: "object",
            card: None,
            tint: String::new(),
        };
        let html = inject("<head><title>fkit</title></head>", &meta, "https://e.test", "fkit");
        assert_eq!(html.matches("<title>").count(), 1);
        assert!(html.contains("<title>owner/repo</title>"), "{html}");
    }

    #[test]
    fn a_page_with_nothing_public_to_say_offers_no_preview() {
        let out = inject_blank("<head><title>fkit</title></head>");
        for tag in ["og:", "twitter:", "oembed", "og:image"] {
            assert!(!out.contains(tag), "{tag} leaked into a blank head: {out}");
        }
        assert!(out.contains(r#"content="noindex""#), "{out}");
        // The shell still has to boot for whoever is allowed to see the page.
        assert!(out.contains("<title>fkit</title>"), "{out}");
    }

    #[test]
    fn a_tint_is_stable_and_a_valid_colour() {
        let a = tint("helba/fkit");
        assert_eq!(a, tint("helba/fkit"), "same name, same colour");
        assert_eq!(a, tint("Helba/FKit"), "case is not part of the identity");
        assert_eq!(a.len(), 7, "{a}");
        assert!(a.starts_with('#'), "{a}");
        assert!(u32::from_str_radix(&a[1..], 16).is_ok(), "{a}");
    }

    #[test]
    fn near_identical_names_get_far_apart_colours() {
        // The point of colouring a repository is telling it from its neighbour.
        let names = ["helba/fkit", "helba/fkit2", "helba/fkil", "helba/loom"];
        let hues: Vec<u32> = names
            .iter()
            .map(|n| u32::from_str_radix(&tint(n)[1..], 16).unwrap())
            .collect();
        for i in 0..hues.len() {
            for j in i + 1..hues.len() {
                assert_ne!(hues[i], hues[j], "{} and {} collide", names[i], names[j]);
            }
        }
    }

    #[test]
    fn every_hue_stays_inside_the_gamut_and_off_the_extremes() {
        // Fixed Oklch lightness is the reason a hue sweep is safe to use as a
        // background-independent accent; check no hue clips to black or white.
        for h in (0..360).step_by(5) {
            let hex = oklch_to_hex(TINT_L, TINT_C, h as f32);
            let v = u32::from_str_radix(&hex[1..], 16).unwrap();
            let (r, g, b) = (v >> 16, (v >> 8) & 0xff, v & 0xff);
            let max = r.max(g).max(b);
            let min = r.min(g).min(b);
            assert!(max > 90, "hue {h} is too dark: {hex}");
            assert!(min < 230, "hue {h} is too washed out: {hex}");
        }
    }

    #[test]
    fn the_page_colour_reaches_the_head() {
        let meta = Meta {
            title: "helba/fkit".into(),
            description: String::new(),
            url: "https://e.test/helba/fkit".into(),
            image: None,
            og_type: "object",
            card: None,
            tint: "#c07a2f".into(),
        };
        let html = inject("<head><title>x</title></head>", &meta, "https://e.test", "fkit hub");
        assert!(html.contains(r##"<meta name="theme-color" content="#c07a2f">"##), "{html}");
        assert!(html.contains(r#"content="fkit hub""#), "{html}");
    }

    #[test]
    fn wrapping_respects_the_line_budget() {
        let long = "word ".repeat(200);
        let lines = wrap(&long, 28.0, 1056.0, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines.last().unwrap().ends_with('…'));
        for l in &lines {
            assert!(fits(l, 28.0, 1056.0), "{l:?} does not fit");
        }
    }

    #[test]
    fn wrapping_a_short_string_marks_no_truncation() {
        let lines = wrap("a short description", 28.0, 1056.0, 3);
        assert_eq!(lines, vec!["a short description"]);
    }

    #[test]
    fn an_unbroken_word_is_cut_rather_than_overflowing() {
        let lines = wrap(&"x".repeat(500), 28.0, 1056.0, 2);
        for l in &lines {
            assert!(fits(l, 28.0, 1056.0), "{l:?} does not fit");
        }
    }

    #[test]
    fn truncate_keeps_it_under_the_limit() {
        let s = "a ".repeat(400);
        assert!(truncate(&s, 300).chars().count() <= 300);
        assert_eq!(truncate("short", 300), "short");
    }

    #[test]
    fn a_card_renders_to_a_png() {
        let png = render_png(&Card {
            tint: tint("something/fkit"),
            eyebrow: "something /".into(),
            title: "fkit".into(),
            body: "content-addressed version control".into(),
            facts: vec![("7 branches".into(), String::new())],
            footer: "44a6dbbfda04c11461de0216967d1827ede150a0".into(),
            badge: Some(Badge::plain("public")),
        })
        .expect("rendered");
        assert_eq!(&png[1..4], b"PNG");
        assert!(png.len() > 1000, "suspiciously small: {}", png.len());
    }
}
