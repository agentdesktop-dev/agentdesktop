use std::env;

use anyhow::{Context, Result, bail};

mod schema;

enum Task {
    Schema,
}

fn task() -> Result<Task> {
    let argument = env::args()
        .nth(1)
        .context("argument is missing; example usage: cargo xtask schema")?;
    match argument.as_str() {
        "schema" => Ok(Task::Schema),
        _ => bail!("unknown task: {argument}"),
    }
}

fn main() -> Result<()> {
    match task()? {
        Task::Schema => schema::generate(),
    }
}
