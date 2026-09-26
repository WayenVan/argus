//! `argus send`: types a prompt into an agent, optionally waiting until it
//! can take one and until the turn it starts is over.

use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use argus_proto::msg::{Availability, Request, Response};
use serde::Serialize;

use crate::client::Conn;
use crate::errors::{self, NOT_READY, coded};
use crate::output::{self, AgentView, OneAgent};
use crate::wait;
use crate::watcher::Watcher;

pub struct SendOptions {
    /// The prompt; `-` reads it from stdin.
    pub text: String,
    pub enter: bool,
    pub force: bool,
    /// Wait until the agent can take the prompt instead of failing.
    pub wait: bool,
    /// After sending, block until the agent is waiting on someone again.
    pub then_wait: bool,
    pub timeout: Option<u64>,
    pub json: bool,
}

/// Types a prompt into an agent, but only while it waits for one; the
/// manager decides, so the check and the typing cannot race.
pub fn send(target: String, opts: SendOptions) -> Result<()> {
    let text = if opts.text == "-" {
        let mut text = String::new();
        io::Read::read_to_string(&mut io::stdin(), &mut text).context("reading the prompt from stdin")?;
        let trimmed = text.trim_end_matches(['\n', '\r']).len();
        text.truncate(trimmed);
        text
    } else {
        opts.text
    };
    let deadline = opts.timeout.map(|s| Instant::now() + Duration::from_secs(s));
    let mut conn = Conn::connect()?;
    if !opts.wait && !opts.then_wait {
        if !opts.json {
            return conn.request(&Request::Send { target, text, enter: opts.enter, force: opts.force }).map(drop);
        }
        let id = conn.find(&target)?.id.to_string();
        let reply = conn.request(&Request::Send { target: id.clone(), text, enter: opts.enter, force: opts.force })?;
        output::print(SentAgent { agent: AgentView::new(&conn.find(&id)?), turn: sent_turn(&reply) });
        return Ok(());
    }

    // Watch before sending, so no activity change after the send is missed.
    let agent = conn.find(&target)?;
    let mut watcher = Watcher::start(Some(vec![agent.id]), true)?;
    let mut current = watcher.get(agent.id).cloned().unwrap_or(agent);
    let request = Request::Send { target: current.id.to_string(), text, enter: opts.enter, force: opts.force };
    let turn = loop {
        if opts.force || current.activity.awaits_prompt() {
            match conn.request(&request) {
                Ok(reply) => break sent_turn(&reply),
                Err(e) if opts.wait && errors::has_code(&e, NOT_READY) => {}
                Err(e) => return Err(e),
            }
        } else if !opts.wait {
            let why = current.activity.send_refusal().unwrap_or_default();
            return Err(coded(NOT_READY, format!("{} {why}", current.name)));
        }
        // Someone typing clears without an activity change, so poll too.
        wait::next_state(&mut watcher, &mut current, deadline, Duration::from_secs(1))?;
    };
    if !opts.then_wait {
        if opts.json {
            output::print(SentAgent { agent: AgentView::new(&current), turn });
        }
        return Ok(());
    }
    let Some(turn) = turn else {
        bail!("sent, but the manager predates turn counting and cannot wait for the turn; run `argus manager restart`");
    };
    let pickup = wait::Pickup { sent: current.clone(), at: Instant::now() };
    let goal = wait::Goal::Availability(Availability::Free);
    let agent = wait::after_turn(&mut watcher, &current, turn, &goal, deadline, Some(pickup), !opts.json)?;
    if opts.json {
        output::print(OneAgent::new(&agent));
    } else {
        println!("{}", agent.activity);
    }
    Ok(())
}

/// `argus send --json`: the agent, and its `turns` when the prompt went in
/// (`null` from a manager that predates turn counting).
#[derive(Serialize)]
struct SentAgent<'a> {
    agent: AgentView<'a>,
    turn: Option<u64>,
}

/// The turn a `Send` reply names; an older manager answers plain `Ok`.
fn sent_turn(reply: &Response) -> Option<u64> {
    match reply {
        Response::Sent { turn } => Some(*turn),
        _ => None,
    }
}
