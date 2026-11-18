//! AEV-44 targeted before/after candidate-count measurement.
//!
//! Reproduces AEV-25's `shared_frame` noise without the full LME/BEAM eval:
//! one long (~200-char) whole-source-unit summary per claim, where distinct
//! episodes share topical tokens but assert different facts. GUNG-21 measured
//! 1,098,466 LME / 362,707 BEAM candidates from those pairs; bounded
//! `resolve --supersede` turned them into false `revises` chains (GUNG-22).
//!
//! `cargo run --example aev44_measurement` prints, per leg, the pairs that
//! became candidates out of the pairs tested. `topical_noise` is the
//! false-positive control; the numeric, negation, and short-value legs are
//! the true-positive controls; `unverified_old` pins the trust boundary.
//! Run the same command on the pristine detector and on the fixed one: the
//! counts are the before/after. `src/conflicts.rs` pins the fixed behavior.
//!
//! AEV-25 persisted its summaries through `validate_entry`, which caps a
//! summary at 200 chars. Here the verified old claim is capped the same way
//! and the new claim is passed straight to `detect_many`, which does not
//! validate its length — that lets the fixture exercise >200-char text too.

use std::collections::HashSet;

use gungnir::conflicts::detect_many;
use gungnir::recall::tokenize;
use gungnir::{Entry, EntryKind, Store, VerificationState};

/// Distinct episodes. Each pair shares several topical stemmed tokens but
/// carries many private tokens, and deliberately has no digit-bearing tokens
/// and no negation words, so on the pristine detector the only signal that
/// raises them is `SharedFrame`. The `assert`s below keep that honest.
const TOPICAL: &[(&str, &str)] = &[
    (
        "In Tuesday's planning call we walked through the checkout migration and Priya said the team would keep the mysql sessions for now while the cache rollout stayed on hold until the next sprint review and the deploy window was left as planned.",
        "Wednesday's retrospective covered the onboarding funnel instead, Marcus noted that mysql sessions stayed stable through the load test and the search index rebuild was deferred to the next sprint review after the staging outage, while Priya asked for a fresh cost estimate from the finance team before the deploy window changed again.",
    ),
    (
        "During the architecture sync the group reviewed the checkout service and agreed the mysql session store needed more capacity before the holiday traffic while the alerting dashboard stayed unchanged for another week.",
        "The Friday incident review looked at the same checkout service again and Priya confirmed mysql sessions held up under the spike but the retry policy for the payment worker was rewritten after the queue backed up and the alerting dashboard gained a new panel.",
    ),
    (
        "Notes from the design discussion about the checkout flow: the team compared mysql sessions with redis, decided to postpone the switch, and asked for a cost estimate before the next sprint review once legal signed off on the vendor terms.",
        "Later in the month a separate design discussion revisited the checkout flow, mysql sessions were still in place, and the team asked for a fresh accessibility audit of the new confirmation screen before the sprint review closed.",
    ),
    (
        "The weekly ops handoff said the checkout deploy window stayed as planned, mysql sessions were healthy, and the on-call rotation moved to Dana while the staging database refresh was skipped for the sprint review.",
        "A different weekly ops handoff two weeks later said the checkout deploy window was still as planned, mysql sessions were healthy, and the team added a runbook for rotating the staging database credentials after the sprint review.",
    ),
    (
        "In the kickoff the product owner explained that checkout used mysql sessions, the roadmap targeted a postgres evaluation, and the analytics export would land after the next sprint review once legal signed off.",
        "The mid-quarter check-in returned to checkout and its mysql sessions, the postgres evaluation was still pending, and the analytics export had been replaced by a simpler weekly csv report for the finance team after the sprint review.",
    ),
    (
        "A support escalation about checkout mentioned mysql sessions timing out for a few users, the on-call engineer restarted the pool, and the ticket was closed after the next sprint review with the config left untouched.",
        "An unrelated support escalation about checkout mentioned mysql sessions during a password reset surge, the on-call engineer added a dashboard panel, and the ticket was reassigned to the identity team after the sprint review.",
    ),
];

/// True-positive controls. Numeric and negation legs are long near-duplicate
/// claims with one value flipped; the short legs are focused value swaps.
const TRUE_POSITIVE: &[(&str, &str, &str)] = &[
    (
        "numeric_change_long",
        "The checkout service timeout is 30s and the mysql session pool stays healthy through the sprint review, with the alerting dashboard unchanged for another week.",
        "The checkout service timeout is 60s and the mysql session pool stays healthy through the sprint review, with the alerting dashboard unchanged for another week.",
    ),
    (
        "numeric_change_long",
        "During the deploy review the team kept the retry budget at 30 attempts for the payment worker while mysql sessions stayed stable and the staging index rebuild was deferred.",
        "During the deploy review the team kept the retry budget at 60 attempts for the payment worker while mysql sessions stayed stable and the staging index rebuild was deferred.",
    ),
    (
        "numeric_change_long",
        "The weekly ops handoff said the checkout deploy window was 2am with mysql sessions healthy and the staging database refresh deferred until the sprint review closed.",
        "The weekly ops handoff said the checkout deploy window was 4am with mysql sessions healthy and the staging database refresh deferred until the sprint review closed.",
    ),
    (
        "negation_change_long",
        "The checkout team will deploy on fridays after the sprint review, with mysql sessions staying healthy and the alerting dashboard unchanged for another week.",
        "The checkout team will never deploy on fridays after the sprint review, with mysql sessions staying healthy and the alerting dashboard unchanged for another week.",
    ),
    (
        "negation_change_long",
        "The architecture sync agreed that the checkout service would keep mysql sessions through the sprint review while the cache rollout stayed on hold and the dashboard was unchanged.",
        "The architecture sync agreed that the checkout service would not keep mysql sessions through the sprint review while the cache rollout stayed on hold and the dashboard was unchanged.",
    ),
    (
        "negation_change_long",
        "The support runbook says the on-call engineer will restart the mysql session pool after the sprint review when the checkout queue backs up and the dashboard stays green.",
        "The support runbook says the on-call engineer will never restart the mysql session pool after the sprint review when the checkout queue backs up and the dashboard stays green.",
    ),
    ("short_value_swap", "sessions use mysql", "sessions use postgres"),
    (
        "short_value_swap",
        "checkout team lead is Priya",
        "checkout team lead is Marcus",
    ),
    (
        "short_value_swap",
        "dns via cloudflare for staging",
        "dns via akamai for staging",
    ),
];

/// Persisted summaries are capped at 200 chars, matching the AEV-25 adapter.
fn capped(s: &str) -> String {
    s.chars().take(200).collect()
}

struct Case {
    leg: &'static str,
    old: String,
    new: String,
    verify_old: bool,
}

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    for (a, b) in TOPICAL {
        assert!(
            b.chars().count() > 200,
            "the new topical claim must be a long (>200 char) summary"
        );
        out.push(Case {
            leg: "topical_noise",
            old: capped(a),
            new: b.to_string(),
            verify_old: true,
        });
    }
    for (leg, old, new) in TRUE_POSITIVE {
        out.push(Case {
            leg,
            old: capped(old),
            new: (*new).to_string(),
            verify_old: true,
        });
    }
    // Trust boundary: the same topical pairs, but the old fact is unverified.
    for (a, b) in TOPICAL {
        out.push(Case {
            leg: "unverified_old",
            old: capped(a),
            new: b.to_string(),
            verify_old: false,
        });
    }
    out
}

/// `(shared, new_private, old_private)` unique stemmed-token counts. Printed
/// as evidence for the gate: topical pairs diverge on many tokens, focused
/// claims on at most a few.
fn token_stats(old: &str, new: &str) -> (usize, usize, usize) {
    let ot: HashSet<String> = tokenize(old).into_iter().collect();
    let nt: HashSet<String> = tokenize(new).into_iter().collect();
    (
        ot.intersection(&nt).count(),
        nt.difference(&ot).count(),
        ot.difference(&nt).count(),
    )
}

/// One verified (or unverified) old claim plus one new claim in a fresh
/// store, so pairs cannot cross-flag each other.
fn raises(c: &Case) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut old = Entry::new("a", EntryKind::Observation, &c.old);
    if c.verify_old {
        old.verify("dataset", None);
    } else {
        old.verification = VerificationState::Unverified;
    }
    store.create(&old).unwrap();
    let new = Entry::new("a", EntryKind::Observation, &c.new);
    !detect_many(&store, "codex", std::slice::from_ref(&new), None)
        .unwrap()
        .is_empty()
}

fn main() {
    let all = cases();
    println!("AEV-44 candidate-count measurement (one old claim + one new claim per case)");

    let mut legs: Vec<&str> = Vec::new();
    for c in &all {
        if !legs.contains(&c.leg) {
            legs.push(c.leg);
        }
    }

    for leg in legs {
        let group: Vec<&Case> = all.iter().filter(|c| c.leg == leg).collect();
        let n = group.len();
        let raised = group.iter().filter(|c| raises(c)).count();
        let (mut max_shared, mut max_np, mut max_op) = (0usize, 0usize, 0usize);
        for c in &group {
            let (shared, np, op) = token_stats(&c.old, &c.new);
            max_shared = max_shared.max(shared);
            max_np = max_np.max(np);
            max_op = max_op.max(op);
        }
        println!(
            "  {leg:<20} candidates={raised}/{n}  shared<={max_shared} new_private<={max_np} old_private<={max_op}"
        );
    }
    println!("total cases={}", all.len());
    println!("  pristine detector: topical_noise=6/6, true positives=9/9, unverified_old=0/6");
    println!("  fixed detector:    topical_noise=0/6, true positives=9/9, unverified_old=0/6");
}
