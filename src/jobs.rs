//! Background worker: every network call happens on one dedicated thread, so
//! the UI never freezes while a ticket or an avatar is being fetched.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};

use crate::avatar::{self, AvatarImage};
use crate::lookup::{self, Details, GameHit, Presence, Target};
use crate::roblox::{self, CheckError, Destination, LaunchOutcome, Limits, Profile};

pub enum Job {
    Validate {
        id: String,
        cookie: String,
    },
    Launch {
        /// Local account id, to tell the window tracker whose launch this was.
        id: String,
        label: String,
        place: Destination,
        cookie: String,
        multi: bool,
        limits: Limits,
        /// Part of a batch: the UI summarises those once the last one is done.
        in_batch: bool,
    },
    Avatar {
        user_id: u64,
    },
    Servers {
        place_id: u64,
    },
    /// Online / in-game status of several users, asked with one account's cookie.
    Presence {
        cookie: String,
        user_ids: Vec<u64>,
    },
    /// Age, Robux and Premium of one account.
    Details {
        id: String,
        user_id: u64,
        cookie: String,
    },
    SearchGames {
        query: String,
    },
    /// Where a player is playing right now.
    FindPlayer {
        cookie: String,
        who: String,
    },
}

pub enum Done {
    Validated {
        id: String,
        result: Result<Profile, CheckError>,
    },
    Launched {
        id: String,
        label: String,
        in_batch: bool,
        result: Result<LaunchOutcome, String>,
    },
    Avatar {
        user_id: u64,
        result: Result<AvatarImage, String>,
    },
    Servers {
        place_id: u64,
        result: Result<Vec<roblox::ServerInfo>, String>,
    },
    Presence {
        result: Result<HashMap<u64, Presence>, String>,
    },
    Details {
        id: String,
        result: Result<Details, String>,
    },
    Games {
        result: Result<Vec<GameHit>, String>,
    },
    Player {
        result: Result<Target, String>,
    },
}

pub struct Worker {
    jobs: Sender<Job>,
    results: Receiver<Done>,
}

impl Worker {
    /// Spawn the worker thread. Jobs are handled strictly one by one.
    pub fn spawn() -> Self {
        let (job_tx, job_rx) = channel::<Job>();
        let (done_tx, done_rx) = channel::<Done>();

        let _ = std::thread::Builder::new()
            .name("rbm-worker".into())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let done = run(job);
                    if done_tx.send(done).is_err() {
                        break;
                    }
                }
            });

        Worker {
            jobs: job_tx,
            results: done_rx,
        }
    }

    pub fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    pub fn try_recv(&self) -> Option<Done> {
        self.results.try_recv().ok()
    }
}

fn run(job: Job) -> Done {
    match job {
        Job::Validate { id, cookie } => Done::Validated {
            id,
            result: roblox::fetch_identity(&cookie),
        },
        Job::Launch {
            id,
            label,
            place,
            cookie,
            multi,
            limits,
            in_batch,
        } => {
            // The id names the Sober profile on Linux, so it has to survive the
            // move into the result.
            let result = roblox::launch_game(&id, place, &cookie, multi, limits);
            Done::Launched {
                id,
                label,
                in_batch,
                result,
            }
        }
        Job::Avatar { user_id } => Done::Avatar {
            user_id,
            result: avatar::load(user_id),
        },
        Job::Servers { place_id } => Done::Servers {
            place_id,
            result: roblox::fetch_servers(place_id),
        },
        Job::Presence { cookie, user_ids } => Done::Presence {
            result: lookup::fetch_presence(&cookie, &user_ids),
        },
        Job::Details {
            id,
            user_id,
            cookie,
        } => Done::Details {
            id,
            result: lookup::fetch_details(&cookie, user_id),
        },
        Job::SearchGames { query } => Done::Games {
            result: lookup::search_games(&query),
        },
        Job::FindPlayer { cookie, who } => Done::Player {
            result: lookup::find_player(&cookie, &who),
        },
    }
}
