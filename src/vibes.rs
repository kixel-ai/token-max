//! `token-max vibes`: the advice the transcripts can't price, apart from `doctor`'s
//! measured lines.
//!
//! Each item is a card naming whose advice it is: a provider's page or the maintainer's
//! pick. An item that names the settings to change is offered as one edit, through
//! `doctor`'s walk and confirm. Nothing here is a verdict or sets an exit code.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

use crate::doctor::{Answer, Card, Change, Event, Fix, How, Offers, Selection, Standing, Style};
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

/// One host's advice.
pub struct Host<'a> {
    /// The host's name, when the run covers another host than this one.
    pub label: Option<&'a str>,
    pub items: &'a [Item],
    /// Its items' changes checked against its files, which a walk offers; None off a
    /// terminal, where each change is only named.
    pub offers: Option<&'a Offers>,
    /// How its cards print: its paths from its own home.
    pub style: &'a Style,
}

/// The heading over `count` cards.
fn title(count: usize, style: &Style) -> String {
    let head = match count {
        0 => "token-max vibes: no advice for these settings.".to_string(),
        1 => "token-max vibes: 1 piece of advice the transcripts can't price.".to_string(),
        n => format!("token-max vibes: {n} pieces of advice the transcripts can't price."),
    };
    let mut text = style.spans(&[(&head, BOLD)], style.width, 0, None);
    if count > 0 {
        text += &style.dim(
            "None of it is measured, so none of it is a verdict. Each card names whose advice it is.",
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
    let mut text = title(count, style);
    let mut n = 0;
    for host in hosts {
        let fixes = fixes(host.items);
        for (index, item) in host.items.iter().enumerate() {
            n += 1;
            let change = fixes
                .iter()
                .find(|f| f.origins.contains(&Origin::Vibe(index)))
                .map(|f| named(&f.how, host.style));
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
    write!(output, "{}", title(count, style))?;
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
    format!(
        "token-max vibes has {count} piece{} of advice the transcripts can't price.",
        if count == 1 { "" } else { "s" }
    )
}
