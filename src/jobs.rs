//! Background worker: every network call happens on one dedicated thread, so
//! the UI never freezes while a ticket or an avatar is being fetched.

use std::sync::mpsc::{channel, Receiver, Sender};

use crate::avatar::{self, AvatarImage};
use crate::roblox::{self, CheckError, Destination, LaunchOutcome, Limits, Profile};

pub enum Job {
    Validate {
        id: String,
        cookie: String,
    },
    Launch {
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
}

pub enum Done {
    Validated {
        id: String,
        result: Result<Profile, CheckError>,
    },
    Launched {
        label: String,
        in_batch: bool,
        result: Result<LaunchOutcome, String>,
    },
    Avatar {
        user_id: u64,
        result: Result<AvatarImage, String>,
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
                    let done = match job {
                        Job::Validate { id, cookie } => Done::Validated {
                            id,
                            result: roblox::fetch_identity(&cookie),
                        },
                        Job::Launch {
                            label,
                            place,
                            cookie,
                            multi,
                            limits,
                            in_batch,
                        } => Done::Launched {
                            label,
                            in_batch,
                            result: roblox::launch_game(place, &cookie, multi, limits),
                        },
                        Job::Avatar { user_id } => Done::Avatar {
                            user_id,
                            result: avatar::load(user_id),
                        },
                    };

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
