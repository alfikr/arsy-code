//! `arsy-ide`: the reference thin client, driven from a terminal.
//!
//! It spawns an agent that speaks ACP on stdio, opens a session, and sends
//! each line typed as one turn. Everything it shows came off the pipe; see the
//! library note on why it links against nothing that could tell it more.

use arsy_ide::{explain, prompt, session_id, Client};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

const USAGE: &str = "\
arsy-ide — the reference ACP client

Usage:
  arsy-ide [--workspace <PATH>] [--agent <COMMAND>]

  --workspace <PATH>   workspace the agent opens (default: the current directory)
  --agent <COMMAND>    the agent to run (default: arsy)

Type a line to send it as a turn. `/quit` closes the session; so does EOF.
";

fn main() -> std::process::ExitCode {
    match session() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "arsy-ide: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn session() -> Result<(), String> {
    let mut workspace = ".".to_owned();
    let mut agent = "arsy".to_owned();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(());
            }
            "--workspace" => workspace = arguments.next().ok_or("--workspace needs a path")?,
            "--agent" => agent = arguments.next().ok_or("--agent needs a command")?,
            other => return Err(format!("unexpected argument `{other}`\n{USAGE}")),
        }
    }

    let mut child = Command::new(&agent)
        .args(["--workspace", &workspace])
        .args(["serve", "--protocol", "acp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start `{agent}`: {error}"))?;
    let reader = BufReader::new(child.stdout.take().ok_or("the agent has no stdout")?);
    let writer = child.stdin.take().ok_or("the agent has no stdin")?;
    let mut client = Client::new(reader, writer);

    let initialized = client.call("initialize", json!({"protocolVersion": 1}))?;
    let capabilities = result(initialized.outcome)?;
    println!(
        "connected · protocol {} · session loading {}",
        capabilities["protocolVersion"], capabilities["agentCapabilities"]["loadSession"]
    );

    let opened = client.call("session/new", json!({"cwd": workspace}))?;
    let opened = result(opened.outcome)?;
    let session = session_id(&opened)
        .ok_or("the agent opened a session without naming it")?
        .to_owned();
    println!("session {session}");

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        let Some(line) = lines.next() else {
            break;
        };
        let line = line.map_err(|error| error.to_string())?;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if text == "/quit" {
            break;
        }
        if text == "/cancel" {
            let cancelled = client.call("session/cancel", json!({"sessionId": session}))?;
            match cancelled.outcome {
                Ok(result) => println!("cancelled {} task(s)", result["cancelled"]),
                Err(error) => println!("cancel refused — {}", explain(&error)),
            }
            continue;
        }

        // Chunks are printed as they arrive rather than after the turn: the
        // whole reason the agent streams is so a person can read along.
        let answered = client.call("session/prompt", prompt(&session, text))?;
        for update in &answered.updates {
            print!("{update}");
        }
        println!();
        match answered.outcome {
            Ok(result) => println!("[{}]", result["stopReason"].as_str().unwrap_or("done")),
            Err(error) => println!("[failed] {}", explain(&error)),
        }
    }

    // Closing the pipe is what stops the agent's loop.
    drop(client);
    let _ = child.wait();
    Ok(())
}

fn result(outcome: Result<Value, Value>) -> Result<Value, String> {
    outcome.map_err(|error| explain(&error))
}
