use crate::repository::add;
use cps_common::errors::CpsiError;
use std::io::{self, Write};

pub fn add_repository(url: String) -> Result<(), CpsiError> {
    let repo = add::AddTargetRepository::new(&url)?;

    println!("Repository URL: {}", &url);
    println!("Fingerprint:\n{}", repo.fingerprint);
    print!("\nTrust this key? [y/N] ");
    io::stdout().flush()?;

    let mut input: String = String::new();
    io::stdin().read_line(&mut input)?;

    if matches!(input.trim(), "y" | "Y") {
        repo.save()?;
        println!("done");
    } else {
        println!("canceled.");
    }

    Ok(())
}
