//! The changelog shown at the foot of 选项 → 关于 — what installing the newer release would bring.
//!
//! It only ever describes **another** version, so it appears only while a check has found one (see
//! `update.rs`): with this build already the newest release there is nothing to describe, and the
//! section stays off the page rather than repeating the version shown above it.
//!
//! The notes come from the repository's curated changelog — `CHANGELOG.md` behind an English UI and
//! `CHANGELOG_zh.md` behind a 简体中文 one — so each release reads in the UI's own language:
//!
//! - The release body, which the release workflow builds from the changelog (see
//!   `.github/workflows/release.yml`), holding both languages split by [`MARKER_EN`] / [`MARKER_ZH`].
//!   The update check already reads that release, so the notes arrive with it.
//! - A release published before that workflow step existed carries only GitHub's auto-generated
//!   "Full Changelog" line. There the check reads the version's section out of the repository's own
//!   changelogs and rebuilds the marked body with [`notes_body`].
//!
//! A release that documents itself in neither carries no notes, and the section stays off the page
//! too — this build cannot embed a version newer than itself, so there would be nothing truthful to
//! show.

use super::*;

/// The markers the release workflow writes in front of each language's notes in a release body.
const MARKER_EN: &str = "<!-- rustgrid:en -->";
const MARKER_ZH: &str = "<!-- rustgrid:zh -->";

/// One line of a version's notes, classified so 关于 can style it.
pub(super) enum NoteLine {
    /// A version heading: `0.0.11`, with the release date when the changelog names one.
    Version {
        version: String,
        date: Option<String>,
    },
    /// A group heading (`### Added` / `### 新增`).
    Group(String),
    /// A bullet item, its wrapped continuation lines joined.
    Bullet(String),
    /// A plain paragraph (e.g. the changelog's `First release.`).
    Text(String),
}

/// The newer release's notes for one UI language, parsed into renderable lines. Empty when
/// `release_notes` names no version — a release with no body, or one whose body says nothing — which
/// is what keeps the section off the page (see the module docs).
pub(super) fn notes(language: LanguageSetting, release_notes: Option<&str>) -> Vec<NoteLine> {
    release_notes
        .and_then(|body| release_body_section(body, language))
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// Whether a release body is one the release job wrote. The language markers are its signature, so
/// this is what tells the notes published from the changelog (see `.github/workflows/release.yml`)
/// apart from everything else — most importantly from the `**Full Changelog**: …` line GitHub
/// auto-generates for a release published before that step existed, which is not a changelog and
/// sends the check to the repository's own copy instead (see [`notes_body`]).
pub(super) fn is_release_notes(body: &str) -> bool {
    body.contains(MARKER_EN) || body.contains(MARKER_ZH)
}

/// A release body in the shape the release job publishes, assembled from the two changelogs'
/// sections for `version`.
///
/// The fallback for a release whose own body carries no notes — every release tagged before the
/// workflow read them out of the changelog. The running build cannot embed a version newer than
/// itself, so `update.rs` fetches the repository's changelogs and rebuilds the body here, keeping
/// the render path (which splits it by language) exactly as it is for a published one. `None`
/// unless **both** changelogs document `version`: half a body would show one UI language a
/// different release's entry.
pub(super) fn notes_body(version: &str, changelog_en: &str, changelog_zh: &str) -> Option<String> {
    let english = version_section(changelog_en, version)?;
    let chinese = version_section(changelog_zh, version)?;
    Some(format!(
        "{MARKER_EN}\n{english}\n\n{MARKER_ZH}\n{chinese}\n"
    ))
}

/// The part of a GitHub release body written for one language.
///
/// A body without the markers — a hand-written release, or one published before the changelog was
/// wired into the workflow — is returned whole, so the panel still shows whatever the release said.
pub(super) fn release_body_section(body: &str, language: LanguageSetting) -> Option<String> {
    let marker = match language {
        LanguageSetting::En => MARKER_EN,
        LanguageSetting::ZhCn => MARKER_ZH,
        // The repository keeps only an English and a Chinese changelog, so other language UI reads the
        // English notes rather than showing nothing.
        _ => MARKER_EN,
    };
    let Some(start) = body.find(marker) else {
        let body = body.trim();
        return (!body.is_empty()).then(|| body.to_string());
    };
    let rest = &body[start + marker.len()..];
    let end = rest.find("<!-- rustgrid:").unwrap_or(rest.len());
    let section = rest[..end].trim();
    (!section.is_empty()).then(|| section.to_string())
}

/// The changelog block for one version, as written in the file (heading included).
pub(super) fn version_section(text: &str, version: &str) -> Option<String> {
    version_blocks(text)
        .into_iter()
        .find(|block| block_is_version(block, version))
        .map(|block| block.trim_end().to_string())
}

/// The changelog's blocks, each starting at its `## [version]` heading, in file order (newest
/// first). Everything before the first heading — the file's title and preamble — is dropped.
fn version_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut start = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let heading = line.trim_end_matches(['\r', '\n']).starts_with("## [");
        if heading {
            if let Some(from) = start {
                blocks.push(&text[from..offset]);
            }
            start = Some(offset);
        }
        offset += line.len();
    }
    if let Some(from) = start {
        blocks.push(&text[from..]);
    }
    blocks
}

/// Whether a block is the entry for `version`. The heading is parsed rather than prefix-matched, so
/// `0.0.1` never claims `0.0.11`'s entry.
fn block_is_version(block: &str, version: &str) -> bool {
    version_heading(block).is_some_and(|(found, _date)| found == version)
}

/// A `## [0.0.11] - 2026-10-15` heading, as its version and (optional) date.
fn version_heading(block: &str) -> Option<(String, Option<String>)> {
    let heading = block.lines().next()?.trim_end().strip_prefix("## ")?;
    let rest = heading.strip_prefix('[')?;
    let (version, rest) = rest.split_once(']')?;
    let date = rest
        .trim()
        .strip_prefix('-')
        .map(str::trim)
        .filter(|date| !date.is_empty())
        .map(str::to_string);
    Some((version.trim().to_string(), date))
}

/// Split a version's notes into renderable lines: headings, group headings, bullets (with their
/// wrapped continuation lines joined) and paragraphs.
pub(super) fn parse(text: &str) -> Vec<NoteLine> {
    let mut lines = Vec::new();
    let mut bullet: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("<!--") {
            push_bullet(&mut lines, &mut bullet);
            continue;
        }
        if let Some(heading) = line.strip_prefix("## ") {
            push_bullet(&mut lines, &mut bullet);
            match version_heading(line) {
                Some((version, date)) => lines.push(NoteLine::Version { version, date }),
                None => lines.push(NoteLine::Group(strip_inline(heading))),
            }
        } else if let Some(heading) = line.strip_prefix("### ") {
            push_bullet(&mut lines, &mut bullet);
            lines.push(NoteLine::Group(strip_inline(heading)));
        } else if let Some(item) = line.strip_prefix("- ") {
            push_bullet(&mut lines, &mut bullet);
            bullet = Some(item.trim().to_string());
        } else if let Some(item) = bullet.as_mut() {
            // A wrapped bullet: the changelog keeps its entries inside 100 columns.
            item.push(' ');
            item.push_str(line);
        } else {
            lines.push(NoteLine::Text(strip_inline(line)));
        }
    }
    push_bullet(&mut lines, &mut bullet);
    lines
}

/// Close the bullet being collected, unescaping its inline markdown.
fn push_bullet(lines: &mut Vec<NoteLine>, bullet: &mut Option<String>) {
    if let Some(item) = bullet.take() {
        lines.push(NoteLine::Bullet(strip_inline(&item)));
    }
}

/// Drop the inline markdown a plain-text panel cannot render: `**emphasis**`, `` `code` `` and
/// `[label](target)` keep their text, not their syntax.
fn strip_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(['*', '`', '[']) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if let Some(inner) = tail.strip_prefix('[') {
            match inner.split_once(']') {
                // `[label](url)`: keep the label, skip the target.
                Some((label, after)) if after.starts_with('(') => {
                    out.push_str(label);
                    rest = after.split_once(')').map_or(after, |(_url, rest)| rest);
                }
                _ => {
                    out.push('[');
                    rest = inner;
                }
            }
        } else {
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::super::update;
    use super::super::update::APP_VERSION;
    use super::*;

    /// The repository changelogs, as the release job reads them. 关于 itself no longer embeds them —
    /// it describes only a newer release — but the rule that every version is written into both is
    /// still what gives the release job (and the panel) something to publish, so a test reads them.
    const CHANGELOG_EN: &str = include_str!("../../../../CHANGELOG.md");
    const CHANGELOG_ZH: &str = include_str!("../../../../CHANGELOG_zh.md");

    const SAMPLE: &str = "\
# Changelog

Intro.

## [0.0.2] - 2026-02-02

### Added

- A **bold** point that wraps onto
  the next line, with `code`.
- Another point.

## [0.0.1] - 2026-01-01

First release.
";

    #[test]
    fn a_version_block_stops_at_the_next_heading() {
        let section = version_section(SAMPLE, "0.0.2").expect("0.0.2 has an entry");
        assert!(section.contains("A **bold** point"));
        assert!(!section.contains("First release"));
    }

    #[test]
    fn a_version_never_claims_its_own_prefix() {
        // `0.0.1` must not match `0.0.11`'s heading, which starts with the same characters.
        let changelog = "## [0.0.11] - 2026-03-03\n\n### Added\n\n- eleven\n";
        assert_eq!(version_section(changelog, "0.0.1"), None);
        assert_eq!(
            version_section(changelog, "0.0.11").as_deref(),
            Some(changelog.trim_end())
        );
    }

    #[test]
    fn notes_are_classified_and_unmarked() {
        let notes = parse(version_section(SAMPLE, "0.0.2").unwrap().as_str());
        assert!(matches!(
            notes.first(),
            Some(NoteLine::Version { version, date })
                if version == "0.0.2" && date.as_deref() == Some("2026-02-02")
        ));
        assert!(matches!(&notes[1], NoteLine::Group(group) if group == "Added"));
        match &notes[2] {
            NoteLine::Bullet(item) => assert_eq!(
                item,
                "A bold point that wraps onto the next line, with code."
            ),
            _ => panic!("expected the wrapped bullet to be joined"),
        }
        assert!(matches!(&notes[3], NoteLine::Bullet(item) if item == "Another point."));
    }

    #[test]
    fn a_release_body_is_split_by_language() {
        let body = format!(
            "{MARKER_EN}\n## [9.9.9]\n\n### Added\n\n- english\n\n{MARKER_ZH}\n## [9.9.9]\n\n### 新增\n\n- 中文\n"
        );
        let english = release_body_section(&body, LanguageSetting::En).unwrap();
        assert!(english.contains("- english") && !english.contains("中文"));
        let chinese = release_body_section(&body, LanguageSetting::ZhCn).unwrap();
        assert!(chinese.contains("- 中文") && !chinese.contains("english"));
    }

    #[test]
    fn a_release_body_without_markers_is_used_as_is() {
        let body = "## What's Changed\n\n* fix a thing\n";
        let section = release_body_section(body, LanguageSetting::En).unwrap();
        assert_eq!(section, body.trim());
    }

    #[test]
    fn a_release_body_is_recognised_by_its_markers() {
        // Only the release job's own output counts as notes: the check reads the changelog out of
        // the repository for everything else, so a misdetection here would either skip that
        // fallback (showing the auto-generated line) or waste two requests on every release.
        assert!(is_release_notes(&format!("{MARKER_EN}\n## [9.9.9]\n")));
        assert!(is_release_notes(&format!("{MARKER_ZH}\n## [9.9.9]\n")));
        assert!(!is_release_notes(""));
        assert!(!is_release_notes(
            "**Full Changelog**: https://github.com/yisier/rustgrid/compare/v0.0.10...v0.0.11"
        ));
    }

    #[test]
    fn notes_body_reads_a_version_out_of_both_changelogs() {
        // The fallback path end to end: the check fetches the two repository changelogs for a
        // release whose body has no notes, and 关于 must show that version's entry — not the
        // running one — in whichever language the UI is in.
        let english = "# Changelog\n\n## [0.0.12] - 2026-10-16\n\n### Added\n\n- english point\n\n\
                       ## [0.0.11] - 2026-10-15\n\n### Added\n\n- old point\n";
        let chinese = "# 更新日志\n\n## [0.0.12] - 2026-10-16\n\n### 新增\n\n- 中文要点\n\n\
                       ## [0.0.11] - 2026-10-15\n\n### 新增\n\n- 旧要点\n";
        let body = notes_body("0.0.12", english, chinese).expect("both changelogs document 0.0.12");

        let english_notes = notes(LanguageSetting::En, Some(&body));
        assert!(matches!(
            english_notes.first(),
            Some(NoteLine::Version { version, .. }) if version == "0.0.12"
        ));
        assert!(
            english_notes
                .iter()
                .any(|note| matches!(note, NoteLine::Bullet(item) if item == "english point"))
        );
        assert!(
            !english_notes
                .iter()
                .any(|note| matches!(note, NoteLine::Bullet(item) if item == "old point"))
        );

        let chinese_notes = notes(LanguageSetting::ZhCn, Some(&body));
        assert!(matches!(
            chinese_notes.first(),
            Some(NoteLine::Version { version, .. }) if version == "0.0.12"
        ));
        assert!(
            chinese_notes
                .iter()
                .any(|note| matches!(note, NoteLine::Bullet(item) if item == "中文要点"))
        );
    }

    #[test]
    fn a_release_that_documents_nothing_carries_no_notes() {
        // What keeps the 更新日志 section off the page when the newer release documents itself in
        // neither language: `关于` then has nothing truthful to show — the running build cannot embed
        // an entry for a version it is older than.
        assert!(notes(LanguageSetting::En, None).is_empty());
        assert!(notes(LanguageSetting::En, Some("")).is_empty());
        assert!(notes(LanguageSetting::En, Some("   \n")).is_empty());
        assert!(notes(LanguageSetting::En, Some("<!-- rustgrid:en -->\n\n")).is_empty());
    }

    #[test]
    fn notes_body_needs_both_changelogs() {
        // Half a body would hand one UI language the other release's entry; with none, the check
        // keeps the release body it had rather than showing a mismatched changelog.
        let english = "## [0.0.12]\n\n- english point\n";
        let chinese_old = "## [0.0.11]\n\n- 旧要点\n";
        assert_eq!(notes_body("0.0.12", english, chinese_old), None);
        assert_eq!(notes_body("0.0.12", chinese_old, english), None);
        assert_eq!(notes_body("0.0.9", english, chinese_old), None);
    }

    #[test]
    #[ignore = "network: fetches the repository's changelogs from the GitHub API"]
    fn the_repository_changelogs_feed_the_notes_of_another_version() {
        // The live half of the fallback: the contents API really answers with the file text (the
        // media type and the User-Agent GitHub demands are not something a stub can check), and what
        // comes back can be turned into one release's notes, read back in the UI's own language —
        // which is what `notes` does with a fetched body. The version is taken from the fetched file
        // rather than from this build, which may carry a bump the repository has not seen yet.
        let english = update::fetch_repo_file("CHANGELOG.md").expect("CHANGELOG.md is fetchable");
        let chinese =
            update::fetch_repo_file("CHANGELOG_zh.md").expect("CHANGELOG_zh.md is fetchable");
        let newest = version_blocks(&english)
            .first()
            .and_then(|block| version_heading(block))
            .map(|(version, _)| version)
            .expect("the English changelog documents a version");

        let body = notes_body(&newest, &english, &chinese)
            .unwrap_or_else(|| panic!("both changelogs must document {newest}"));
        for language in [LanguageSetting::En, LanguageSetting::ZhCn] {
            let notes = notes(language, Some(&body));
            assert!(
                matches!(
                    notes.first(),
                    Some(NoteLine::Version { version, .. }) if version.as_str() == newest
                ),
                "{language:?}: the fetched body must document {newest}"
            );
        }
    }

    #[test]
    fn the_published_body_is_read_back_in_the_ui_language() {
        // The exact shape the release job publishes: each changelog's section for the tagged
        // version, behind its language marker (see `.github/workflows/release.yml`). 关于 must read
        // it back as the newer release's notes in the UI's own language.
        let english = version_section(CHANGELOG_EN, "0.0.11").unwrap();
        let chinese = version_section(CHANGELOG_ZH, "0.0.11").unwrap();
        let body = format!("{MARKER_EN}\n{english}\n\n{MARKER_ZH}\n{chinese}\n");
        let english_notes = notes(LanguageSetting::En, Some(&body));
        assert!(matches!(
            english_notes.first(),
            Some(NoteLine::Version { version, .. }) if version == "0.0.11"
        ));
        let chinese_notes = notes(LanguageSetting::ZhCn, Some(&body));
        assert!(matches!(
            chinese_notes.first(),
            Some(NoteLine::Version { version, .. }) if version == "0.0.11"
        ));
        assert!(matches!(&chinese_notes[1], NoteLine::Group(group) if group == "新增"));
    }

    #[test]
    fn the_changelogs_document_the_running_version() {
        // Guards the release rule: bumping the workspace version without writing the entry in both
        // changelogs leaves the release job with nothing to publish — and 关于 with nothing to show
        // for that version while it is the newer release.
        for (name, changelog) in [
            ("CHANGELOG.md", CHANGELOG_EN),
            ("CHANGELOG_zh.md", CHANGELOG_ZH),
        ] {
            let section = version_section(changelog, APP_VERSION)
                .unwrap_or_else(|| panic!("{name} has no entry for version {APP_VERSION}"));
            assert!(
                section.starts_with(&format!("## [{APP_VERSION}]")),
                "{name}'s {APP_VERSION} entry must start at its own heading"
            );
        }
    }

    #[test]
    fn the_english_changelog_is_written_in_english() {
        // Each changelog stays in its own language, so its UI labels read the way that language's
        // `locales/*.yml` has them — 选项 → 关于 is "Options → About" here. The header's language
        // switcher and the name of the other language are the only exceptions.
        const ALLOWED: &str = "简体中文";
        let offenders = CHANGELOG_EN
            .lines()
            .enumerate()
            .filter(|(_, line)| line.replace(ALLOWED, "").chars().any(is_cjk))
            .map(|(index, line)| format!("{}: {}", index + 1, line.trim()))
            .collect::<Vec<_>>();
        assert!(
            offenders.is_empty(),
            "CHANGELOG.md must be English throughout — quote UI labels from locales/en.yml:\n{}",
            offenders.join("\n")
        );
    }

    /// Han characters, CJK punctuation and full-width forms, i.e. text that is not English.
    fn is_cjk(c: char) -> bool {
        matches!(
            c as u32,
            0x3000..=0x303F | 0x3040..=0x30FF | 0x4E00..=0x9FFF | 0xFF00..=0xFFEF
        )
    }
}
