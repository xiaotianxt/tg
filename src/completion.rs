use std::io::{self, Write};

use clap::CommandFactory;
use clap_complete::{generate, shells};

use crate::{Cli, CompletionShell};

pub(crate) fn print_script(shell: CompletionShell) -> Result<(), String> {
    let script = generate_script(shell);
    io::stdout()
        .write_all(&script)
        .map_err(|error| format!("Write completion script: {error}"))
}

fn generate_script(shell: CompletionShell) -> Vec<u8> {
    let mut command = Cli::command();
    let mut script = Vec::new();
    match shell {
        CompletionShell::Fish => generate(shells::Fish, &mut command, "tg", &mut script),
        CompletionShell::Zsh => generate(shells::Zsh, &mut command, "tg", &mut script),
        CompletionShell::Bash => generate(shells::Bash, &mut command, "tg", &mut script),
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_static_scripts_for_every_shell() {
        for shell in [
            CompletionShell::Fish,
            CompletionShell::Zsh,
            CompletionShell::Bash,
        ] {
            let script = String::from_utf8(generate_script(shell)).unwrap();
            assert!(!script.trim().is_empty());
            assert!(script.contains("sessions"));
            assert!(script.contains("anonymous"));
            assert!(!script.contains("__complete"));
        }
    }

    #[test]
    fn scripts_include_clap_possible_values() {
        for shell in [
            CompletionShell::Fish,
            CompletionShell::Zsh,
            CompletionShell::Bash,
        ] {
            let script = String::from_utf8(generate_script(shell)).unwrap();
            assert!(script.contains("newest"));
            assert!(script.contains("oldest"));
        }
    }
}
