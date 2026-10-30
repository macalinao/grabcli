//! `grab`: the Grab command-line client.
//!
//! Composes shared auth commands (`grab auth …`) and per-service commands
//! (`grab food …`, `grab mart …`). A new service slots in as another `#[bpaf(command)]`
//! variant whose parser and `run` live in a module here (`food`, `mart`).

mod auth;
mod cache;
mod food;
mod local;
mod mart;
mod output;
mod paths;
mod places;

use bpaf::Bpaf;
use grabcli_core::cache::CacheMode;

use food::{FoodCmd, food_cmd};
use mart::{MartCmd, mart_cmd};
use output::Output;

use auth::{AuthCmd, auth_cmd};
use cache::{CacheCmd, cache_cmd};
use places::{PlacesCmd, places_cmd};

/// Top-level command groups.
#[derive(Debug, Clone, Bpaf)]
enum Service {
    /// Shared auth: import session, mint hydra, status.
    #[bpaf(command("auth"))]
    Auth {
        #[bpaf(external(auth_cmd))]
        cmd: AuthCmd,
    },
    /// GrabFood: search, menu, cart, quote, vouchers, order, track.
    #[bpaf(command("food"))]
    Food {
        /// Output JSON (also accepted on any subcommand).
        #[bpaf(long("json"), switch)]
        json: bool,
        #[bpaf(external(food_cmd))]
        cmd: FoodCmd,
    },
    /// GrabMart: stores, products, cart, quote, order.
    #[bpaf(command("mart"))]
    Mart {
        /// Output JSON (also accepted on any subcommand).
        #[bpaf(long("json"), switch)]
        json: bool,
        #[bpaf(external(mart_cmd))]
        cmd: MartCmd,
    },
    /// Saved places (Home, Work, …): list, or deliver to one.
    #[bpaf(command("saved-places"))]
    SavedPlaces {
        #[bpaf(external(places_cmd))]
        cmd: PlacesCmd,
    },
    /// Local cache of read-only browse data: stats, clear, prune, path, image.
    #[bpaf(command("cache"))]
    Cache {
        #[bpaf(external(cache_cmd))]
        cmd: CacheCmd,
    },
}

#[derive(Debug, Clone, Bpaf)]
#[bpaf(options, version)]
struct Cli {
    /// Output JSON for machine use (structured errors on stderr).
    #[bpaf(long("json"), switch)]
    json: bool,
    /// Don't read or write the response cache (also GRAB_NO_CACHE=1).
    #[bpaf(long("no-cache"), switch)]
    no_cache: bool,
    /// Ignore cached responses but store the fresh ones.
    #[bpaf(long("refresh"), switch)]
    refresh: bool,
    #[bpaf(external(service))]
    service: Service,
}

#[tokio::main]
async fn main() {
    let cli = cli().run();
    let cache_mode = CacheMode::from_flags(cli.no_cache, cli.refresh);

    // --json is honoured at the top level, at the service level, and on the
    // leaf command, so it works wherever the user puts it.
    let (out, result) = match cli.service {
        Service::Auth { cmd } => {
            let out = Output::new(cli.json || auth::wants_json(&cmd));
            (out, auth::run(out, cmd).await)
        }
        Service::Food { json, cmd } => {
            let out = Output::new(cli.json || json || food::cmd_wants_json(&cmd));
            (out, food::run(out, cache_mode, cmd).await)
        }
        Service::Mart { json, cmd } => {
            let out = Output::new(cli.json || json || mart::cmd_wants_json(&cmd));
            (out, mart::run(out, cache_mode, cmd).await)
        }
        Service::SavedPlaces { cmd } => {
            let out = Output::new(cli.json || places::wants_json(&cmd));
            (out, places::run(out, cmd).await)
        }
        Service::Cache { cmd } => {
            let out = Output::new(cli.json || cache::wants_json(&cmd));
            (out, cache::run(out, cmd).await)
        }
    };

    if let Err(err) = result {
        std::process::exit(out.emit_error(&err));
    }
}
