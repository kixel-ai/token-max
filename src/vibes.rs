//! `token-max vibes`: the advice the transcripts can't price, apart from `doctor`'s
//! measured lines.
//!
//! Each item is a card naming whose advice it is: a provider's page or the maintainer's
//! pick. An item that names the settings to change is offered as one edit, through
//! `doctor`'s walk and confirm. Nothing here is a verdict or sets an exit code.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

use crate::doctor::{
    Answer, Card, Change, Event, Fix, How, Offers, Owner, Selection, Standing, Style,
};
use crate::model::Harness;
use crate::report::Origin;
use crate::terminal::BOLD;

/// One piece of advice, as its card shows it. Its text is ready to print: paths already
/// read from the home directory of the host it is about.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    /// The setting or feature it is about, with its profile when the run covers several.
    pub name: String,
    /// The current value and the advised one, such as "10m → 4m"; empty when it names none.
    pub value: String,
    /// What the setting does; empty when the advice says so itself.
    pub what: String,
    /// The advice, with what was measured beside it.
    pub text: String,
    /// Whose advice it is: the provider's pages, or the maintainer's pick.
    pub sources: Vec<String>,
    /// The settings change it names, which the walk offers; None for a card with no question.
    pub how: Option<How>,
    /// Nothing to act on, such as a setting already as advised: `vibes` shows it, and
    /// `doctor` does not offer `vibes` for it.
    pub quiet: bool,
    /// The card to show instead once `doctor`'s walk writes the change it offers for a line.
    pub after: Option<After>,
}

/// What a card becomes once `doctor` writes one of its lines' changes, since that change
/// moves the setting the card advises on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct After {
    /// The `doctor` line whose change moves the setting.
    pub origin: Origin,
    pub item: Box<Item>,
}

/// The window the maintainer caps compaction at for `harness`, for output quality rather than
/// cost.
pub fn window_cap(harness: Harness) -> i64 {
    match harness {
        Harness::Claude => 400_000,
        Harness::Codex => 240_000,
    }
}

/// `items` as they read once `doctor`'s walk has written the changes of the lines `written`.
pub fn settled(items: Vec<Item>, written: &[Origin]) -> Vec<Item> {
    items
        .into_iter()
        .map(|item| match item.after {
            Some(after) if written.contains(&after.origin) => *after.item,
            _ => Item {
                after: None,
                ..item
            },
        })
        .collect()
}

/// How many of `items` are worth `doctor` offering `vibes` for.
pub fn pending(items: &[Item]) -> usize {
    items.iter().filter(|i| !i.quiet).count()
}

/// The changes `items` name as the walk's fixes, each tied to its item. Two items that make
/// one edit share a fix, and the second of two values for one setting is left by hand.
pub fn fixes(items: &[Item]) -> Vec<Fix> {
    let fixes = items.iter().enumerate().filter_map(|(index, item)| {
        Some(Fix {
            name: item.name.clone(),
            value: item.value.clone(),
            suggested: true,
            saving: None,
            margin: None,
            verb: "saves",
            share: None,
            what: String::new(),
            evidence: String::new(),
            how: item.how.clone()?,
            origins: vec![Origin::Vibe(index)],
            group: None,
            removal: vec![],
        })
    });
    crate::doctor::fixes_from(fixes.collect())
}

/// The host that owns `items`' changes, each checked against its file as it is now. `home`
/// and `monthly` are that host's.
pub fn owner(items: &[Item], home: Option<String>, monthly: f64) -> Owner {
    Owner::new(fixes(items), vec![], home, monthly)
}

/// `items` with each change as the walk would offer it, for a host that only prints them.
/// Resolved on the host whose files they name, since a symlink there can make two files one.
pub fn resolved(mut items: Vec<Item>) -> Vec<Item> {
    let fixes = fixes(&items);
    for (index, item) in items.iter_mut().enumerate() {
        item.how = fixes
            .iter()
            .find(|f| f.origins.contains(&Origin::Vibe(index)))
            .map(|f| f.how.clone());
    }
    items
}

/// One host's advice.
pub struct Host<'a> {
    /// The host's name, when the run covers another host than this one.
    pub label: Option<&'a str>,
    /// Its cards, [`resolved`] on that host when no walk offers them.
    pub items: &'a [Item],
    /// Its items' changes checked against its files, which a walk offers; None off a
    /// terminal, where each change is only named.
    pub offers: Option<&'a Offers>,
    /// How its cards print: its paths from its own home.
    pub style: &'a Style,
}

/// The heading over the cards, counting the advice as `doctor`'s offer does: a card with
/// nothing to act on is shown and not counted.
fn title(hosts: &[Host<'_>], style: &Style) -> String {
    let count: usize = hosts.iter().map(|h| pending(h.items)).sum();
    let head = match count {
        0 => "token-max vibes: no advice for these settings.".to_string(),
        n => format!("token-max vibes: {}.", pieces(n)),
    };
    let mut text = style.spans(&[(&head, BOLD)], style.width, 0, None);
    if count > 0 {
        text += &style.dim(
            "None of it is measured, so none of it is a verdict. Each card names whose advice it is.",
        );
    }
    // Said once, as `doctor` does, for every extrapolated figure a card quotes.
    let extrapolated = hosts
        .iter()
        .flat_map(|h| h.items)
        .any(|i| crate::report::sampled(&i.text));
    if extrapolated {
        text += &style.dim(
            "A ~ marks a figure extrapolated from the requests that could be priced to those that could not, assuming they resemble each other, followed by its 95% margin of error.",
        );
    }
    text
}

/// An item's card, its counter `counter`, with `change` naming the edit it offers or the
/// step it leaves by hand.
fn card(item: &Item, counter: String, host: &Host<'_>, change: Option<String>) -> String {
    let mut rows = vec![];
    if let Some(label) = host.label {
        rows.push(("Host", label.to_string()));
    }
    if !item.what.is_empty() {
        rows.push(("What", item.what.clone()));
    }
    rows.push(("Advice", item.text.clone()));
    rows.push(("Source", item.sources.join(" ")));
    if let Some(change) = change {
        rows.push(("Change", change));
    }
    host.style.card(&Card {
        counter,
        name: &item.name,
        saving: None,
        no_saving: "not measured",
        value: item.value.clone(),
        standing: Standing::Advice,
        rows,
    })
}

/// A change as its card names it: the edit and its file, or the step by hand and why.
fn named(how: &How, style: &Style) -> String {
    match how {
        How::Edit(patch) => {
            let edits: Vec<String> = patch.edits.iter().map(|e| e.text(patch.harness)).collect();
            format!("{} in {}", edits.join(" and "), style.file(&patch.file))
        }
        How::Manual { step, reason } => crate::doctor::home_in(
            &format!("by hand, since {reason}: {step}"),
            style.home.as_deref(),
        ),
    }
}

/// Every host's cards as `vibes` prints them off a terminal, each change named and none made.
pub fn text(hosts: &[Host<'_>], style: &Style) -> String {
    let count: usize = hosts.iter().map(|h| h.items.len()).sum();
    let mut text = title(hosts, style);
    let mut n = 0;
    for host in hosts {
        for item in host.items {
            n += 1;
            let change = item.how.as_ref().map(|how| named(how, host.style));
            text += &format!("\n{}", card(item, format!("[{n}/{count}]"), host, change));
        }
    }
    text
}

/// Walk through every host's cards, asking about each change a card offers, and return each
/// host's answers. A card with no change is shown with no question. A quit, or end of input,
/// shows no more cards and declines what is left.
pub fn ask(
    hosts: &[Host<'_>],
    style: &Style,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<Vec<Selection>> {
    let count: usize = hosts.iter().map(|h| h.items.len()).sum();
    write!(output, "{}", title(hosts, style))?;
    let offered = hosts
        .iter()
        .any(|h| h.offers.is_some_and(|o| o.offered() > 0));
    if offered {
        write!(
            output,
            "{}",
            style.dim("Nothing is written until you confirm the list at the end.")
        )?;
    }
    let mut selections: Vec<Selection> = hosts.iter().map(|_| Selection::default()).collect();
    let mut n = 0;
    let mut going = true;
    for (host, selection) in hosts.iter().zip(&mut selections) {
        let Some(offers) = host.offers else { continue };
        // Each offer answered so far: two items can name one edit.
        let mut answered = vec![];
        for (index, item) in host.items.iter().enumerate() {
            n += 1;
            let change = offers.change(Origin::Vibe(index));
            let offer = match change {
                Some(Change::Offer(offer, _)) => Some(offer),
                _ => None,
            };
            if !going {
                continue;
            }
            let named = match &change {
                Some(Change::Offer(_, fix)) => Some(named(&fix.how, host.style)),
                Some(Change::ByHand(step)) => Some(step.to_string()),
                None => None,
            };
            write!(
                output,
                "\n{}",
                card(item, format!("[{n}/{count}]"), host, named)
            )?;
            let Some(offer) = offer else { continue };
            if answered.contains(&offer) {
                write!(
                    output,
                    "{}",
                    host.style.dim("The same edit as a card above.")
                )?;
                continue;
            }
            answered.push(offer);
            let answer =
                crate::doctor::answer(&host.style.prompt("Accept?", false), input, output)?;
            let yes = answer == Answer::Yes;
            selection
                .apply(offers, Event::Choose { offer, yes })
                .expect("the walk answers only what it offers");
            going = answer != Answer::Quit;
        }
    }
    Ok(selections)
}

/// Ask, when `doctor`'s walk ends, whether to show the `count` pieces of advice `vibes` has.
pub fn offer(
    count: usize,
    style: &Style,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<bool> {
    let show = if count == 1 { "Show it?" } else { "Show them?" };
    crate::doctor::yes_or_no(&format!("{} {show}", hint(count)), style, input, output)
}

/// What `doctor` says of `vibes`' `count` pieces of advice, which it leaves out.
pub fn hint(count: usize) -> String {
    format!("token-max vibes has {}.", pieces(count))
}

/// `count` pieces of advice, as the heading and `doctor` both count them.
fn pieces(count: usize) -> String {
    format!(
        "{} of advice the transcripts can't price",
        crate::report::plural(count as u64, "piece")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{Edit, Op, Patch};
    use crate::test_support::Workspace;
    use serde_json::json;

    fn style(w: &Workspace) -> Style {
        Style {
            color: false,
            unicode: false,
            width: None,
            home: Some(w.home.to_string_lossy().into_owned()),
            host: None,
        }
    }

    /// A card with no change to make.
    fn card(name: &str) -> Item {
        Item {
            name: name.into(),
            value: String::new(),
            what: String::new(),
            text: format!("{name} is advised against."),
            sources: vec!["https://example.com/guide".into()],
            how: None,
            quiet: false,
            after: None,
        }
    }

    /// A card that sets the timeout variables named, in the profile's settings.
    fn cap(w: &Workspace, name: &str, variables: &[&str], value: &str) -> Item {
        let edits = variables
            .iter()
            .map(|variable| Edit {
                path: vec!["env".into(), variable.to_string()],
                value: json!(value),
                op: Op::Set,
            })
            .collect();
        Item {
            value: "10m -> 4m".into(),
            what: "How long a foreground command may run.".into(),
            how: Some(How::Edit(Patch {
                file: w
                    .claude
                    .join("settings.json")
                    .to_string_lossy()
                    .into_owned(),
                harness: Harness::Claude,
                edits,
            })),
            ..card(name)
        }
    }

    /// Walk `items` on this host with `answers` typed, returning what was printed and each
    /// answer recorded.
    fn walk(w: &Workspace, items: &[Item], answers: &str) -> (String, Vec<Event>, Owner) {
        let style = style(w);
        let owner = Owner::new(fixes(items), vec![], style.home.clone(), 100.0);
        let hosts = [Host {
            label: None,
            items,
            offers: Some(owner.offers()),
            style: &style,
        }];
        let mut output = vec![];
        let selections = ask(&hosts, &style, &mut answers.as_bytes(), &mut output).unwrap();
        let events = selections[0].events().to_vec();
        (String::from_utf8(output).unwrap(), events, owner)
    }

    const MAX: &str = "BASH_MAX_TIMEOUT_MS";
    const DEFAULT: &str = "BASH_DEFAULT_TIMEOUT_MS";

    #[test]
    fn only_a_card_that_names_its_settings_asks_and_two_keys_are_one_confirmed_edit() {
        let w = Workspace::new();
        std::fs::write(w.claude.join("settings.json"), "{}\n").unwrap();
        let items = [
            card("Main-session effort"),
            cap(&w, "Bash timeout cap", &[MAX, DEFAULT], "240000"),
            card("Fable weekly use"),
        ];
        let (text, events, owner) = walk(&w, &items, "y\n");
        let shown = format!("~{0}.claude{0}settings.json", std::path::MAIN_SEPARATOR);
        // One question, for the one card with a change; the others end at their source.
        assert_eq!(text.matches("Accept? [y]es, [n]o, [q]uit:").count(), 1);
        assert!(
            text.starts_with("token-max vibes: 3 pieces of advice the transcripts can't price.\n  None of it is measured, so none of it is a verdict. Each card names whose advice it is.\n  Nothing is written until you confirm the list at the end.\n\n  [1/3] Main-session effort  not measured\n        Advice  Main-session effort is advised against.\n        Source  https://example.com/guide\n\n  [2/3] Bash timeout cap  not measured\n        10m -> 4m\n        What    How long a foreground command may run.\n"),
            "{text}"
        );
        assert!(
            text.contains(&format!("        Change  \"env\": {{\"BASH_MAX_TIMEOUT_MS\": \"240000\"}} and \"env\": {{\"BASH_DEFAULT_TIMEOUT_MS\": \"240000\"}} in {shown}\n  Accept? [y]es, [n]o, [q]uit: \n  [3/3] Fable weekly use  not measured\n")),
            "{text}"
        );
        // No card carries a verdict.
        assert!(!text.contains("Recommended") && !text.contains("Worth considering"));
        assert_eq!(
            events,
            [Event::Choose {
                offer: 0,
                yes: true
            }]
        );
        // The yes is one change to one file, though it writes two keys.
        let plan = owner.plan(&events).unwrap();
        let written = crate::doctor::write(&plan).unwrap();
        assert_eq!(written.len(), 1);
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(w.claude.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings, json!({"env": {MAX: "240000", DEFAULT: "240000"}}));
    }

    #[test]
    fn a_quit_shows_no_more_cards_and_accepts_nothing_more() {
        let w = Workspace::new();
        std::fs::write(w.claude.join("settings.json"), "{}\n").unwrap();
        let other = w.home.join("other.json");
        std::fs::write(&other, "{}\n").unwrap();
        let mut second = cap(&w, "Second cap", &[MAX], "240000");
        if let Some(How::Edit(patch)) = &mut second.how {
            patch.file = other.to_string_lossy().into_owned();
        }
        let items = [
            cap(&w, "First cap", &[MAX], "240000"),
            second,
            card("Fable weekly use"),
        ];
        for (answers, shown, accepted) in [
            ("q\n", 1, vec![false]),
            // End of input is a quit.
            ("y\n", 2, vec![true, false]),
            ("n\ny\n", 3, vec![false, true]),
        ] {
            let (text, events, _) = walk(&w, &items, answers);
            assert_eq!(text.matches("not measured").count(), shown, "{answers:?}");
            let yes: Vec<bool> = events
                .iter()
                .map(|e| matches!(e, Event::Choose { yes: true, .. }))
                .collect();
            assert_eq!(yes, accepted, "{answers:?}");
        }
    }

    #[test]
    fn cards_that_share_an_edit_ask_once_and_a_clashing_or_unwritable_one_is_by_hand() {
        let w = Workspace::new();
        std::fs::write(w.claude.join("settings.json"), "{}\n").unwrap();
        let items = [
            cap(&w, "Cap", &[MAX], "240000"),
            cap(&w, "Same cap", &[MAX], "240000"),
            cap(&w, "Other cap", &[MAX], "3300000"),
        ];
        let (text, events, _) = walk(&w, &items, "y\n");
        assert_eq!(text.matches("Accept?").count(), 1, "{text}");
        let shown = format!("~{0}.claude{0}settings.json", std::path::MAIN_SEPARATOR);
        assert!(
            text.contains(&format!("in {shown}\n  The same edit as a card above.\n")),
            "{text}"
        );
        assert!(
            text.contains(&format!("        Change  by hand, since Cap wants a different value for the same setting: in {shown}, set \"env\": {{\"BASH_MAX_TIMEOUT_MS\": \"3300000\"}}\n")),
            "{text}"
        );
        assert_eq!(events.len(), 1);
        // Off a terminal the same changes are named, and nothing is asked or read.
        let style = style(&w);
        // Resolved where the files are, as each host does before it prints or sends them.
        let items = resolved(items.to_vec());
        let hosts = [Host {
            label: Some("devbox"),
            items: &items,
            offers: None,
            style: &style,
        }];
        let printed = super::text(&hosts, &style);
        assert!(!printed.contains("Accept?") && !printed.contains("Nothing is written"));
        assert_eq!(printed.matches("        Host    devbox\n").count(), 3);
        assert_eq!(
            printed
                .matches(&format!(
                    "        Change  \"env\": {{\"BASH_MAX_TIMEOUT_MS\": \"240000\"}} in {shown}\n"
                ))
                .count(),
            2,
            "{printed}"
        );
        assert!(
            printed.contains("        Change  by hand, since Cap wants"),
            "{printed}"
        );
        // A file that cannot take the edit leaves the walk a step by hand, with no question.
        std::fs::write(w.claude.join("settings.json"), "not json").unwrap();
        let (text, events, _) = walk(&w, &items[..1], "y\n");
        assert!(text.contains("        Change  by hand, since "), "{text}");
        assert!(!text.contains("Accept?") && !text.contains("Nothing is written"));
        assert_eq!(events, []);
    }

    #[test]
    fn an_extrapolated_figure_on_a_card_states_its_assumption_once() {
        let w = Workspace::new();
        let style = style(&w);
        let print = |items: &[Item]| {
            let hosts = [Host {
                label: None,
                items,
                offers: None,
                style: &style,
            }];
            text(&hosts, &style)
        };
        const ASSUMED: &str = "assuming they resemble each other";
        // A path under ~ is no figure.
        let exact = Item {
            text: "It cost $2.00, set in ~/.claude/settings.json.".into(),
            ..card("Exact")
        };
        assert!(!print(std::slice::from_ref(&exact)).contains(ASSUMED));
        let sampled = Item {
            text: "It cost ~$2.00 \u{b1} $0.50.".into(),
            ..card("Sampled")
        };
        let printed = print(&[exact.clone(), sampled.clone(), sampled]);
        assert_eq!(printed.matches(ASSUMED).count(), 1, "{printed}");
        // A figure under a cent, or a negative one, is marked the same way.
        for figure in ["~<$0.01", "~-$2.00", "~-<$0.01"] {
            let small = Item {
                text: format!("It cost {figure}, margin unknown."),
                ..card("Small")
            };
            assert!(print(&[exact.clone(), small]).contains(ASSUMED), "{figure}");
        }
    }

    #[test]
    fn nothing_to_advise_says_so_and_quiet_cards_are_not_worth_an_offer() {
        let w = Workspace::new();
        let style = style(&w);
        assert_eq!(
            super::text(&[], &style),
            "token-max vibes: no advice for these settings.\n"
        );
        let quiet = Item {
            quiet: true,
            ..card("Bash timeout cap")
        };
        assert_eq!(pending(&[quiet, card("Fable weekly use")]), 1);
        assert_eq!(
            hint(1),
            "token-max vibes has 1 piece of advice the transcripts can't price."
        );
        let mut output = vec![];
        // End of input is a no.
        assert!(!offer(2, &style, &mut "".as_bytes(), &mut output).unwrap());
        assert!(offer(2, &style, &mut "yes\n".as_bytes(), &mut output).unwrap());
        assert!(
            String::from_utf8(output).unwrap().starts_with(
                "\n  token-max vibes has 2 pieces of advice the transcripts can't price. Show them? [y]es, [n]o: "
            )
        );
    }
}
