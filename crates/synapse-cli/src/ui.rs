use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

/// Prints the modern Synapse-Mux brand header.
pub fn print_banner() {
    println!(
        "\n{} {} {}",
        "⚡".yellow(),
        "SYNAPSE-MUX".bold().cyan(),
        "│ Contract-First Cross-Model Orchestrator".dimmed()
    );
    println!("{}\n", "─".repeat(60).dimmed());
}

/// Interactive step progress manager with spinners.
pub struct StepProgress {
    current_step: usize,
    total_steps: usize,
    spinner: Option<ProgressBar>,
    is_tty: bool,
}

impl StepProgress {
    pub fn new(total_steps: usize) -> Self {
        Self {
            current_step: 0,
            total_steps,
            spinner: None,
            is_tty: io::stdout().is_terminal(),
        }
    }

    pub fn start_step(&mut self, title: &str) {
        self.current_step += 1;
        let prefix = format!("[{}/{}]", self.current_step, self.total_steps);

        if self.is_tty {
            let pb = ProgressBar::new_spinner();
            pb.enable_steady_tick(Duration::from_millis(80));
            pb.set_style(
                ProgressStyle::default_spinner()
                    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
                    .template(&format!(
                        "{{spinner:.cyan}} {} {{msg}}",
                        prefix.bold().blue()
                    ))
                    .unwrap(),
            );
            pb.set_message(title.to_string());
            self.spinner = Some(pb);
        } else {
            println!("{} {}", prefix.bold().blue(), title);
        }
    }

    pub fn complete_step(&mut self, detail: &str) {
        let prefix = format!("[{}/{}]", self.current_step, self.total_steps);
        if let Some(pb) = self.spinner.take() {
            pb.finish_and_clear();
        }
        println!(
            "{} {} {}",
            "✔".green().bold(),
            prefix.bold().dimmed(),
            detail.bold()
        );
    }

    pub fn fail_step(&mut self, detail: &str) {
        let prefix = format!("[{}/{}]", self.current_step, self.total_steps);
        if let Some(pb) = self.spinner.take() {
            pb.finish_and_clear();
        }
        println!(
            "{} {} {}",
            "✖".red().bold(),
            prefix.bold().red(),
            detail.red().bold()
        );
    }

    pub fn log_substep(&self, message: &str) {
        println!("   {} {}", "├──".dimmed(), message);
    }

    pub fn log_substep_final(&self, message: &str) {
        println!("   {} {}", "└──".dimmed(), message);
    }
}

/// Renders a structured summary card inside an ASCII/Unicode box.
pub fn print_card(title: &str, fields: &[(&str, String)], notes: Option<&[&str]>) {
    let width = 64;
    let border_top = format!("┌{}┐", "─".repeat(width - 2));
    let border_sep = format!("├{}┤", "─".repeat(width - 2));
    let border_bottom = format!("└{}┘", "─".repeat(width - 2));

    println!("{}", border_top.dimmed());
    println!("│ {:<width$} │", title.bold().cyan(), width = width - 4);
    println!("{}", border_sep.dimmed());

    for (k, v) in fields {
        let label = format!("{}:", k);
        let content_width = width - 4 - 18;
        let truncated = if v.len() > content_width {
            format!("{}...", &v[..content_width.saturating_sub(3)])
        } else {
            v.clone()
        };
        println!(
            "│ {:<16} {:<content_width$} │",
            label.dimmed(),
            truncated,
            content_width = content_width
        );
    }

    if let Some(note_lines) = notes {
        println!("{}", border_sep.dimmed());
        for line in note_lines {
            let content_width = width - 4;
            println!(
                "│ {:<content_width$} │",
                line.dimmed(),
                content_width = content_width
            );
        }
    }

    println!("{}\n", border_bottom.dimmed());
}

/// Formats a prominent action-oriented error card.
pub fn print_error_box(headline: &str, details: &str, suggestion: &str) {
    println!("\n{}", "─".repeat(60).red());
    println!("{} {}", "✖ ERROR:".bold().red(), headline.bold().white());
    println!("{}", "─".repeat(60).dimmed());
    println!("{}\n", details);
    println!("{} {}", "💡 SUGGESTED ACTION:".bold().yellow(), suggestion);
    println!("{}\n", "─".repeat(60).red());
}

/// Prompts the user interactively with a [Y/n] confirmation.
/// Falls back to `default` when stdin is not an interactive terminal.
pub fn prompt_confirm(prompt_msg: &str, default: bool) -> bool {
    if !io::stdin().is_terminal() {
        return default;
    }

    let default_hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{} {} ", prompt_msg.bold(), default_hint.cyan());
    io::stdout().flush().unwrap_or(());

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return default;
    }

    let trimmed = input.trim().to_lowercase();
    if trimmed.is_empty() {
        return default;
    }

    trimmed == "y" || trimmed == "yes"
}
