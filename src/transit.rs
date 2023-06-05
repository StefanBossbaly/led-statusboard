use crate::{config::TransitConfig, render::Render};
use anyhow::{anyhow, Context, Result};
use embedded_graphics::{
    mono_font::{self, MonoTextStyle},
    pixelcolor::Rgb888,
    prelude::{Point, RgbColor},
    text::{Alignment, Text},
    Drawable,
};
use geoutils::{Distance, Location};
use home_assistant_rest::get::StateEnum;
use log::{debug, warn};
use parking_lot::Mutex;
use septa_api::{responses::Train, types::RegionalRailStop};
use std::{
    collections::HashMap,
    error::Error,
    fs::File,
    io::BufReader,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use strum::IntoEnumIterator;
use tokio::{join, task::JoinHandle};

/// The amount of time the user has to be within the radius of a station to be considered at the station.
const NO_STATUS_TO_AT_STATION: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
struct NoStatusTracker {
    /// A map of the regional rail stop to the time the user entered into the radius of the station.
    /// We use time::Instance since we need a monotonic clock and do not care about the system time.
    station_to_first_encounter: HashMap<RegionalRailStop, Instant>,
}

#[derive(Debug, Default)]
struct TrainEncounter {
    /// The first time the user encountered the train inside the radius of the current station.
    first_encounter_inside_station: Option<Instant>,

    /// The first time the user encountered the train outside the radius of the current station.
    first_encounter_outside_station: Option<Instant>,
}

// Have to wrap in lazy_static since from_meters is not a const function.
lazy_static! {
    /// The radius around a station that a user must be within to be considered at the station.
    static ref AT_STATION_ENTER_RADIUS: Distance = Distance::from_meters(200.0);
}

/// The amount of time that a user would need to be outside a station's radius to
/// transition from AtStation to NoStatus.
const AT_STATION_TO_NO_STATUS_TIMEOUT: Duration = Duration::from_secs(60);

// Have to wrap in lazy_static since from_meters is not a const function.
lazy_static! {
    static ref AT_STATION_LEAVE_RADIUS: Distance = Distance::from_meters(200.0);
}

#[derive(Debug)]
struct StationTracker {
    /// The station the user is currently at.
    station: RegionalRailStop,

    /// A map from the unique train id to the time the user encountered the train within the radius
    /// of a station and the time the user encountered the train outside the radius of a station.
    train_id_to_first_encounter: HashMap<String, TrainEncounter>,

    /// The time the user has been outside the radius of the station.
    time_outside_station: Option<Instant>,
}

const ON_TRAIN_TO_NO_STATUS_TIMEOUT: Duration = Duration::from_secs(300);

// Have to wrap in lazy_static since from_meters is not a const function.
lazy_static! {
    static ref ON_TRAIN_ENTER_RADIUS: Distance = Distance::from_meters(400.0);
    static ref ON_TRAIN_REMAIN_RADIUS: Distance = Distance::from_meters(400.0);
}

#[derive(Debug)]
struct TrainTracker {
    /// The unique train id.
    train_id: String,

    /// The time the user has been on the train.
    last_train_encounter: Instant,
}

#[derive(Debug)]
enum State {
    NoStatus(NoStatusTracker),
    AtStation(StationTracker),
    OnTrain(TrainTracker),
}

#[derive(Default)]
pub(crate) struct TransitState {
    state: Option<State>,
}

impl TransitState {
    fn new() -> Self {
        Self { state: None }
    }

    fn update_state(&mut self, lat_lon: (f64, f64), trains: Vec<Train>) -> Result<()> {
        // Get the monotonic time
        let now = Instant::now();
        let person_location = Location::new(lat_lon.0, lat_lon.1);

        // We will now take ownership of self.state into the local state value. Since it
        // possible for self.state to be None, we will populate that with a State::NoStatus
        // since it means that this is the first time this function was called. Once we have
        // the state moved out we are now able to reassign self.state to the new value in the
        // state machine. The Option allows us to use &mut self instead of mut self where we
        // consume the current instance and then have to result Self as a result. I also prefer
        // this way over using std::memory::replace(), that function, even though it is safe, seems
        // like a can of worms that once opened won't be able to be closed easily. Using .take() on
        // the Option is more idiomatic Rust.
        let state = match self.state.take() {
            Some(state) => state,
            None => State::NoStatus(NoStatusTracker::default()),
        };

        self.state = Some(match state {
            State::NoStatus(mut tracker) => {
                let mut eligible_stations = Vec::new();

                // See if we are currently in any station's radius
                for station in
                    RegionalRailStop::iter().filter(|p| !matches!(p, RegionalRailStop::Unknown(_)))
                {
                    let station_lat_lon = station.lat_lon()?;
                    let station_location = Location::new(station_lat_lon.0, station_lat_lon.1);

                    if person_location
                        .is_in_circle(&station_location, *AT_STATION_ENTER_RADIUS)
                        .expect("is_in_circle failed")
                    {
                        match tracker.station_to_first_encounter.get(&station) {
                            Some(first_encounter) => {
                                if now - *first_encounter > NO_STATUS_TO_AT_STATION {
                                    eligible_stations.push(station);
                                }
                            }
                            None => {
                                tracker
                                    .station_to_first_encounter
                                    .insert(station.clone(), now);
                            }
                        }
                    } else {
                        // We are not in the radius of the station, so remove it from the map
                        tracker.station_to_first_encounter.remove(&station);
                    }
                }

                // Iterate over eligible stations, if there are more than one, pick the closest one
                match eligible_stations.len() {
                    0 => State::NoStatus(tracker),
                    1 => {
                        let station = eligible_stations[0].clone();

                        debug!(
                            "Transitioning from NoStatus to AtStation (station: {})",
                            station.to_string()
                        );
                        State::AtStation(StationTracker {
                            station,
                            train_id_to_first_encounter: HashMap::new(),
                            time_outside_station: None,
                        })
                    }
                    _ => {
                        let mut closest_station = eligible_stations[0].clone();
                        let closest_lat_lon = closest_station.lat_lon()?;
                        let mut closest_distance: Distance = person_location
                            .distance_to(&Location::new(closest_lat_lon.0, closest_lat_lon.1))
                            .map_err(|e| anyhow!("distance_to failed: {}", e))?;

                        for station in eligible_stations {
                            let station_lat_lon = station.lat_lon()?;
                            let distance = person_location
                                .distance_to(&Location::new(station_lat_lon.0, station_lat_lon.1))
                                .map_err(|e| anyhow!("distance_to failed: {}", e))?;

                            if distance.meters() < closest_distance.meters() {
                                closest_station = station;
                                closest_distance = distance;
                            }
                        }

                        debug!(
                            "Transitioning from NoStatus to AtStation for (station: {})",
                            closest_station.to_string()
                        );
                        State::AtStation(StationTracker {
                            station: closest_station,
                            train_id_to_first_encounter: HashMap::new(),
                            time_outside_station: None,
                        })
                    }
                }
            }
            State::AtStation(mut tracker) => {
                let station_location = {
                    let station_lat_lon = tracker.station.lat_lon()?;
                    Location::new(station_lat_lon.0, station_lat_lon.1)
                };

                // See if we are still at the current location
                let mut is_outside_location = false;
                if person_location
                    .is_in_circle(&station_location, *AT_STATION_LEAVE_RADIUS)
                    .map_err(|e| anyhow!("distance_to failed: {}", e))?
                {
                    // We are still at the station, so update the time we have been outside the station
                    tracker.time_outside_station = None;
                } else {
                    // We are no longer at the station, so update the time we have been outside the station
                    match tracker.time_outside_station {
                        Some(first_left) => {
                            if now - first_left > AT_STATION_TO_NO_STATUS_TIMEOUT {
                                is_outside_location = true;
                            }
                        }
                        None => {
                            tracker.time_outside_station = Some(now);
                        }
                    }
                }

                if is_outside_location {
                    debug!(
                        "Transitioning from AtStation to NoStatus (station: {})",
                        tracker.station.to_string()
                    );
                    State::NoStatus(NoStatusTracker::default())
                } else {
                    // See if we are in the radius of any train
                    let mut matched_train = None;
                    'train_loop: for train in trains {
                        let train_location = Location::new(train.lat, train.lon);
                        if person_location
                            .is_in_circle(&train_location, *ON_TRAIN_ENTER_RADIUS)
                            .map_err(|e| anyhow!("distance_to failed: {}", e))?
                        {
                            match tracker
                                .train_id_to_first_encounter
                                .get_mut(&train.train_number)
                            {
                                Some(train_encounters) => {
                                    let currently_at_station = person_location
                                        .is_in_circle(&train_location, *AT_STATION_LEAVE_RADIUS)
                                        .map_err(|e| anyhow!("distance_to failed: {}", e))?;

                                    if currently_at_station {
                                        train_encounters.first_encounter_inside_station =
                                            train_encounters
                                                .first_encounter_inside_station
                                                .or(Some(now));
                                    } else {
                                        train_encounters.first_encounter_outside_station =
                                            train_encounters
                                                .first_encounter_outside_station
                                                .or(Some(now));
                                    }

                                    // We have to have at least one encounter inside the station and one outside the station
                                    // TODO: Have some sort of time component to this transition
                                    if train_encounters.first_encounter_inside_station.is_some()
                                        && train_encounters
                                            .first_encounter_outside_station
                                            .is_some()
                                    {
                                        matched_train = Some(train.train_number);
                                        break 'train_loop;
                                    }
                                }
                                None => {
                                    let mut train_encounters = TrainEncounter {
                                        first_encounter_inside_station: None,
                                        first_encounter_outside_station: None,
                                    };

                                    let currently_at_station = person_location
                                        .is_in_circle(&train_location, *AT_STATION_LEAVE_RADIUS)
                                        .map_err(|e| anyhow!("distance_to failed: {}", e))?;

                                    if currently_at_station {
                                        train_encounters.first_encounter_inside_station =
                                            train_encounters
                                                .first_encounter_inside_station
                                                .or(Some(now));
                                    } else {
                                        train_encounters.first_encounter_outside_station =
                                            train_encounters
                                                .first_encounter_outside_station
                                                .or(Some(now));
                                    }

                                    tracker
                                        .train_id_to_first_encounter
                                        .insert(train.train_number, train_encounters);
                                }
                            }
                        } else {
                            // We are not in the radius of the train, so remove it from the map
                            tracker
                                .train_id_to_first_encounter
                                .remove(&train.train_number);
                        }
                    }

                    match matched_train {
                        Some(train_id) => {
                            debug!(
                                "Transitioning from AtStation to OnTrain (station: {}, train: {})",
                                tracker.station.to_string(),
                                train_id
                            );
                            State::OnTrain(TrainTracker {
                                train_id,
                                last_train_encounter: now,
                            })
                        }
                        None => State::AtStation(tracker),
                    }
                }
            }
            State::OnTrain(mut tracker) => {
                // See if we are still in the radius of the train
                let current_train = trains
                    .iter()
                    .find(|&train| train.train_number == tracker.train_id);

                match current_train {
                    Some(train) => {
                        let train_location = Location::new(train.lat, train.lon);
                        if person_location
                            .is_in_circle(&train_location, *ON_TRAIN_REMAIN_RADIUS)
                            .map_err(|e| anyhow!("distance_to failed: {}", e))?
                        {
                            tracker.last_train_encounter = now;
                            State::OnTrain(tracker)
                        } else if tracker.last_train_encounter - now > ON_TRAIN_TO_NO_STATUS_TIMEOUT
                        {
                            let station: Option<RegionalRailStop> = {
                                let mut regional_rail_stop = None;
                                for station in RegionalRailStop::iter() {
                                    let station_location = {
                                        let (lat, lon) = station.lat_lon()?;
                                        Location::new(lat, lon)
                                    };
                                    if person_location
                                        .is_in_circle(&station_location, *AT_STATION_ENTER_RADIUS)
                                        .map_err(|e| anyhow!("distance_to failed: {}", e))?
                                    {
                                        regional_rail_stop = Some(station);
                                        break;
                                    }
                                }

                                regional_rail_stop
                            };

                            match station {
                                Some(station) => {
                                    debug!(
                                        "Transitioning from OnTrain to AtStation (station: {}, train: {})",
                                        station.to_string(), tracker.train_id
                                    );
                                    State::AtStation(StationTracker {
                                        station,
                                        time_outside_station: None,
                                        train_id_to_first_encounter: HashMap::new(),
                                    })
                                }
                                None => {
                                    debug!(
                                        "Transitioning from OnTrain to NoStatus (train: {})",
                                        tracker.train_id
                                    );
                                    State::NoStatus(NoStatusTracker::default())
                                }
                            }
                        } else {
                            State::OnTrain(tracker)
                        }
                    }
                    None => State::NoStatus(NoStatusTracker {
                        station_to_first_encounter: HashMap::new(),
                    }),
                }
            }
        });

        Ok(())
    }
}

#[derive(Default)]
struct StateHolder {
    transit_state: TransitState,
    person_name: Option<String>,
    person_state: Option<String>,
}

pub(crate) struct TransitRender {
    state: Arc<Mutex<StateHolder>>,
    /// Flag used to gracefully terminate the render and driver threads
    alive: Arc<AtomicBool>,
    /// Handle to the task used to update the SEPTA and User location
    update_task_handle: Option<JoinHandle<Result<()>>>,
}

impl TransitRender {
    const CONFIG_FILE: &'static str = "transit.yaml";

    fn get_config_file() -> Result<File> {
        let home_dir = std::env::var("HOME").context("Can not load HOME environment variable")?;
        let mut file_path = PathBuf::from(home_dir);
        file_path.push(Self::CONFIG_FILE);
        File::open(file_path).with_context(|| format!("Failed to open file {}", Self::CONFIG_FILE))
    }

    fn read_config() -> Result<TransitConfig> {
        let file_reader = BufReader::new(Self::get_config_file()?);
        serde_yaml::from_reader(file_reader).context("Unable to parse YAML file")
    }

    async fn get_location(
        home_assistant_client: &home_assistant_rest::Client,
        config: &TransitConfig,
    ) -> Result<(Option<String>, Option<String>, f64, f64)> {
        let entity_state = home_assistant_client
            .get_states_of_entity(&config.person_entity_id)
            .await?;

        // Attempt to get the person's name
        let person_name = if let Some(state_value) = entity_state.attributes.get("friendly_name") {
            if let Some(value) = state_value.as_str() {
                Some(value.to_owned())
            } else {
                warn!("Could not parse 'friendly_name' as str");
                None
            }
        } else {
            warn!("Could find 'friendly_name' in attributes");
            None
        };

        // Attempt to get the person's state
        let person_state = if let Some(state_value) = entity_state.state {
            if let StateEnum::String(value) = state_value {
                Some(value)
            } else {
                warn!("Could not parse 'state' as str");
                None
            }
        } else {
            warn!("{}'s 'state' was not provided", config.person_entity_id);
            None
        };

        if let (Some(lat), Some(lon)) = (
            entity_state.attributes.get("latitude"),
            entity_state.attributes.get("longitude"),
        ) {
            if let (Some(lat_f64), Some(lon_f64)) = (lat.as_f64(), lon.as_f64()) {
                Ok((person_name, person_state, lat_f64, lon_f64))
            } else {
                Err(anyhow!("Could not match lat lng"))
            }
        } else {
            Err(anyhow!("Could not match lat lng"))
        }
    }

    pub(crate) fn new(config: TransitConfig) -> Result<Self, Box<dyn Error>> {
        let septa_client = septa_api::Client::new();
        let home_assistant_client = home_assistant_rest::Client::new(
            &config.home_assistant_url,
            &config.home_assistant_bearer_token,
        )?;

        let state_holder = Arc::new(Mutex::new(StateHolder::default()));
        let alive = Arc::new(AtomicBool::new(true));

        // Clone the shared data since it will be moved onto the task
        let task_state_holder = state_holder.clone();
        let task_alive = alive.clone();

        let update_task_handle: JoinHandle<Result<()>> = tokio::task::spawn(async move {
            while task_alive.load(Ordering::SeqCst) {
                let trains_request = septa_client.train_view();
                let user_location_request = Self::get_location(&home_assistant_client, &config);

                let (trains_result, user_location_result) =
                    join!(trains_request, user_location_request);

                let (person_name, person_state, user_loc_lat, user_loc_lon) = user_location_result?;
                let trains = trains_result?;

                {
                    let mut holder_unlocked = task_state_holder.lock();
                    holder_unlocked
                        .transit_state
                        .update_state((user_loc_lat, user_loc_lon), trains)?;

                    holder_unlocked.person_name = person_name;
                    holder_unlocked.person_state = person_state;
                } // drop(holder_unlocked)

                tokio::time::sleep(Duration::from_secs(15)).await;
            }

            Ok(())
        });

        Ok(Self {
            state: state_holder,
            alive,
            update_task_handle: Some(update_task_handle),
        })
    }

    pub(crate) fn from_config() -> Result<Self, Box<dyn Error>> {
        Self::new(Self::read_config()?)
    }
}

impl Render for TransitRender {
    fn render(&self, canvas: &mut rpi_led_panel::Canvas) -> Result<()> {
        let state_unlocked = self.state.lock();

        if let Some(ref state) = state_unlocked.transit_state.state {
            canvas.fill(0, 0, 0);

            // Render the name of the person
            let name = match state_unlocked.person_name {
                Some(ref name) => name,
                None => "Unknown",
            };

            Text::with_alignment(
                name,
                Point::new(0, 15),
                MonoTextStyle::new(&mono_font::ascii::FONT_6X10, Rgb888::WHITE),
                Alignment::Left,
            )
            .draw(canvas)?;

            let status_text = match state {
                State::NoStatus(_) => {
                    if let Some(ref state) = state_unlocked.person_state {
                        state.to_owned()
                    } else {
                        "Unknown".to_owned()
                    }
                }
                State::AtStation(ref tracker) => {
                    format!("At Station {}", tracker.station)
                }
                State::OnTrain(ref tracker) => format!("On Train {}", tracker.train_id),
            };

            Text::with_alignment(
                status_text.as_str(),
                Point::new(0, 40),
                MonoTextStyle::new(&mono_font::ascii::FONT_5X7, Rgb888::WHITE),
                Alignment::Left,
            )
            .draw(canvas)?;
        }

        Ok(())
    }
}

impl Drop for TransitRender {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);

        if let Some(task_handle) = self.update_task_handle.take() {
            task_handle.abort();
        }
    }
}
