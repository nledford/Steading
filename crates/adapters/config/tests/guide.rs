//! The user guide's examples of `config.toml` (#477) are text this parser reads:
//! a new user copies them, and a key it doesn't take would stop the daemon at
//! its next start.

mod support;

use domain::secrets::SecretRef;
use domain::{PlanId, RepositoryAlias};
use support::parsed;

/// The text of a page of the user guide, `docs/guide/<page>.md`.
fn guide(page: &str) -> String {
    let path = format!(
        "{}/../../../docs/guide/{page}.md",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path} is readable: {error}"))
}

/// The text of every page of the user guide, in file-name order.
fn all_guide_pages() -> Vec<String> {
    let folder = format!("{}/../../../docs/guide", env!("CARGO_MANIFEST_DIR"));
    let mut paths: Vec<_> = std::fs::read_dir(&folder)
        .unwrap_or_else(|error| panic!("{folder} is readable: {error}"))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("{} is readable: {error}", path.display()))
        })
        .collect()
}

/// The text of each ```` ```toml ```` block of `markdown`, in order.
fn toml_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut block: Option<String> = None;
    for line in markdown.lines() {
        match block.as_mut() {
            None if line.trim_start() == "```toml" => block = Some(String::new()),
            None => {}
            Some(_) if line.trim_start() == "```" => blocks.extend(block.take()),
            Some(text) => {
                text.push_str(line);
                text.push('\n');
            }
        }
    }
    blocks
}

/// A block as a whole file: one that doesn't say its schema is a part of
/// `config.toml`, which says it first.
fn as_file(block: &str) -> String {
    if block.lines().any(|line| line.starts_with("schema = ")) {
        block.to_owned()
    } else {
        format!("schema = 1\n\n{block}")
    }
}

/// The first plan's blocks that hold `needle`.
fn blocks_with(needle: &str) -> Vec<String> {
    toml_blocks(&guide("first-plan"))
        .into_iter()
        .filter(|block| block.contains(needle))
        .collect()
}

#[test]
fn every_toml_example_in_the_guide_is_a_configuration_the_daemon_reads() {
    let blocks: Vec<String> = all_guide_pages()
        .iter()
        .flat_map(|page| toml_blocks(page))
        .collect();
    assert!(!blocks.is_empty(), "the guide shows no config.toml");
    for block in blocks {
        if let Err(error) = parsed(&as_file(&block)) {
            panic!("the guide's example doesn't parse: {error:?}\n{block}");
        }
    }
}

#[test]
fn the_guides_example_plan_has_sources_a_schedule_a_keep_policy_and_exclusions() {
    let blocks = blocks_with("[[plans]]");
    let [block] = blocks.as_slice() else {
        panic!("the guide shows {} example plans, not one", blocks.len());
    };
    let parsed = parsed(&as_file(block)).expect("the example parses");
    let [plan] = parsed.config.plans() else {
        panic!(
            "the example holds {} plans, not one",
            parsed.config.plans().len()
        );
    };
    assert_eq!(plan.id, PlanId::parse("home").expect("an ID"));
    assert!(!plan.sources.is_empty(), "no sources");
    assert!(plan.schedule.is_some(), "no schedule");
    assert!(!plan.retention.is_empty(), "no keep policy");
    assert!(
        !plan.filter.iglobs.is_empty() && !plan.filter.exclude_if_present.is_empty(),
        "no exclusions"
    );
}

#[test]
fn the_guides_keyring_entries_are_the_ones_its_commands_store() {
    let blocks = blocks_with("password-keyring");
    assert!(!blocks.is_empty(), "the guide shows no keyring entry");
    let nas = RepositoryAlias::parse("nas").expect("an alias");
    for block in blocks {
        let parsed = parsed(&as_file(&block)).expect("the example parses");
        let Some(repository) = parsed.config.repository(&nas) else {
            panic!("the example names no repository nas:\n{block}");
        };
        let SecretRef::Keyring(entry) = &repository.settings.password else {
            panic!("the example's password is not a keyring entry:\n{block}");
        };
        // The secrets adapter asks the OS keyring for the service and the
        // name: on macOS the item's service (`-s`) and account (`-a`), on
        // Linux the Secret Service attributes `service` and `username`.
        assert_eq!((entry.service(), entry.name()), ("ladderback", "nas"));
    }
    let secrets = guide("secrets");
    for command in [
        "security add-generic-password -s ladderback -a nas -w",
        "service ladderback username nas",
    ] {
        assert!(
            secrets.contains(command),
            "the guide doesn't say `{command}`"
        );
    }
}
