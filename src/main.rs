mod browser;
mod youtube;

use anyhow::Result;
use browser::Browser;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "joyscroller", about = "Archive the short videos you liked, forever.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Export the browser's cookies for a domain to a Netscape cookies.txt.
    Cookies {
        #[arg(long, value_enum, default_value = "brave")]
        browser: Browser,
        #[arg(long, default_value = "Default")]
        profile: String,
        #[arg(long, default_value = "youtube.com")]
        domain: String,
        /// Where to write cookies.txt (default: stdout).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// List the videos you've liked on YouTube.
    Likes {
        #[arg(long, value_enum, default_value = "brave")]
        browser: Browser,
        #[arg(long, default_value = "Default")]
        profile: String,
        /// How many to list (0 = all).
        #[arg(long, default_value = "20")]
        limit: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Cookies { browser, profile, domain, out } => {
            let cookies = browser::cookies_for(browser, &profile, &domain).await?;
            let txt = browser::to_netscape(&cookies);
            match out {
                Some(path) => {
                    std::fs::write(&path, txt)?;
                    eprintln!("wrote {} cookies to {}", cookies.len(), path.display());
                }
                None => print!("{txt}"),
            }
        }
        Command::Likes { browser, profile, limit } => {
            let cookies = browser::cookies_for(browser, &profile, "youtube.com").await?;
            eprintln!("extracted {} youtube.com cookies", cookies.len());

            // yt-dlp needs the cookies on disk; keep the temp file for the call's lifetime.
            let tmp = tempfile::Builder::new().suffix(".txt").tempfile()?;
            std::fs::write(tmp.path(), browser::to_netscape(&cookies))?;

            let videos = youtube::liked_videos(tmp.path(), limit).await?;
            eprintln!("\n{} liked videos:\n", videos.len());
            for (i, v) in videos.iter().enumerate() {
                let dur = v
                    .duration_seconds
                    .map(|s| format!("{}:{:02}", s / 60, s % 60))
                    .unwrap_or_else(|| "--:--".into());
                let channel = v.channel.as_deref().unwrap_or("?");
                println!("{:>3}. [{dur}] {} — {}", i + 1, v.title, channel);
                println!("     {}", v.url());
            }
        }
    }
    Ok(())
}
