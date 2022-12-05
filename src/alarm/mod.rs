use crate::action;
use crate::{Error, PlaceholderMap, Result};
use async_trait::async_trait;

mod level;

pub use level::Level;

#[cfg_attr(test, mockall::automock(type Item=u8;))]
pub trait DataSink: Send + Sync + Sized {
    type Item: Send + Sync;

    fn put_data(&mut self, data: &Self::Item) -> Result<SinkDecision>;
    fn format_data(data: &Self::Item) -> String;
}

pub enum SinkDecision {
    Good,
    Bad,
}

impl std::ops::Not for SinkDecision {
    type Output = Self;

    fn not(self) -> Self::Output {
        match self {
            SinkDecision::Good => SinkDecision::Bad,
            SinkDecision::Bad => SinkDecision::Good,
        }
    }
}

#[async_trait]
pub trait Alarm: Send + Sync + Sized {
    type Item: Send + Sync;

    async fn put_data(&mut self, data: &Self::Item, mut placeholders: PlaceholderMap)
        -> Result<()>;
    async fn put_error(&mut self, error: &Error, mut placeholders: PlaceholderMap) -> Result<()>;
}

pub struct AlarmBase<T>
where
    T: DataSink,
{
    name: String,
    id: String,
    action: Option<std::sync::Arc<dyn action::Action>>,
    placeholders: PlaceholderMap,
    cycles: u32,
    repeat_cycles: u32,
    recover_action: Option<std::sync::Arc<dyn action::Action>>,
    recover_placeholders: PlaceholderMap,
    recover_cycles: u32,
    error_action: Option<std::sync::Arc<dyn action::Action>>,
    error_placeholders: PlaceholderMap,
    error_repeat_cycles: u32,
    invert: bool,
    state: State,
    data_sink: T,
}

#[derive(Clone)]
enum State {
    Good(GoodState),
    Bad(BadState),
    Error(ErrorState),
}

impl Default for State {
    fn default() -> Self {
        Self::Good(GoodState::default())
    }
}

#[derive(Clone)]
struct GoodState {
    timestamp: std::time::SystemTime,
    last_alarm_uuid: Option<String>,
    bad_cycles: u32,
}

impl Default for GoodState {
    fn default() -> Self {
        Self {
            timestamp: std::time::SystemTime::now(),
            last_alarm_uuid: None,
            bad_cycles: 0,
        }
    }
}

#[derive(Clone)]
struct BadState {
    timestamp: std::time::SystemTime,
    uuid: String,
    cycles: u32,
    good_cycles: u32,
}

#[derive(Clone)]
struct ErrorState {
    timestamp: std::time::SystemTime,
    uuid: String,
    shadowed_state: Box<State>,
    cycles: u32,
}

impl<T> AlarmBase<T>
where
    T: DataSink,
{
    pub fn new(
        name: String,
        id: String,
        action: Option<std::sync::Arc<dyn action::Action>>,
        placeholders: PlaceholderMap,
        cycles: u32,
        repeat_cycles: u32,
        recover_action: Option<std::sync::Arc<dyn action::Action>>,
        recover_placeholders: PlaceholderMap,
        recover_cycles: u32,
        error_action: Option<std::sync::Arc<dyn action::Action>>,
        error_placeholders: PlaceholderMap,
        error_repeat_cycles: u32,
        invert: bool,
        data_sink: T,
    ) -> Self {
        // TODO ensure cycles != 0 and recover_cycles != 0
        Self {
            name,
            id,
            action,
            placeholders,
            cycles,
            repeat_cycles,
            recover_action,
            recover_placeholders,
            recover_cycles,
            error_action,
            error_placeholders,
            error_repeat_cycles,
            invert,
            state: State::default(),
            data_sink,
        }
    }

    fn error_update_state(&self, state: &State) -> (State, bool) {
        let mut trigger = false;
        let new_state = match state {
            State::Good(_) => {
                trigger = true;
                State::Error(ErrorState {
                    timestamp: std::time::SystemTime::now(),
                    uuid: uuid::Uuid::new_v4().to_string(),
                    shadowed_state: Box::new(state.clone()),
                    cycles: 1,
                })
            }

            State::Bad(_) => {
                trigger = true;
                State::Error(ErrorState {
                    timestamp: std::time::SystemTime::now(),
                    uuid: uuid::Uuid::new_v4().to_string(),
                    shadowed_state: Box::new(state.clone()),
                    cycles: 1,
                })
            }

            State::Error(error) => {
                let cycles = if error.cycles + 1 == self.error_repeat_cycles {
                    trigger = true;
                    1
                } else {
                    error.cycles + 1
                };
                State::Error(ErrorState {
                    timestamp: error.timestamp,
                    uuid: error.uuid.clone(),
                    shadowed_state: error.shadowed_state.clone(),
                    cycles,
                })
            }
        };
        (new_state, trigger)
    }

    fn bad_update_state(&mut self, state: &State) -> (State, bool) {
        let mut trigger = false;
        let new_state = match state {
            State::Good(good) => {
                if good.bad_cycles + 1 == self.cycles {
                    trigger = true;
                    State::Bad(BadState {
                        timestamp: std::time::SystemTime::now(),
                        uuid: uuid::Uuid::new_v4().to_string(),
                        cycles: 1,
                        good_cycles: 0,
                    })
                } else {
                    State::Good(GoodState {
                        timestamp: good.timestamp,
                        last_alarm_uuid: None,
                        bad_cycles: good.bad_cycles + 1,
                    })
                }
            }

            State::Bad(bad) => {
                let cycles = if bad.cycles + 1 == self.repeat_cycles {
                    trigger = true;
                    1
                } else {
                    bad.cycles + 1
                };
                State::Bad(BadState {
                    timestamp: bad.timestamp,
                    uuid: bad.uuid.clone(),
                    cycles,
                    good_cycles: 0,
                })
            }

            State::Error(error) => {
                self.state = *error.shadowed_state.clone();
                let (shadowed_state, shadowed_trigger) =
                    self.bad_update_state(&error.shadowed_state);
                trigger = shadowed_trigger;
                shadowed_state
            }
        };
        (new_state, trigger)
    }

    fn good_update_state(&mut self, state: &State) -> (State, bool) {
        let mut trigger = false;
        let new_state = match state {
            State::Good(good) => State::Good(good.clone()), // TODO maybe unset last_alarm_uuid

            State::Bad(bad) => {
                if bad.good_cycles + 1 == self.recover_cycles {
                    trigger = true;
                    State::Good(GoodState {
                        timestamp: std::time::SystemTime::now(),
                        last_alarm_uuid: Some(bad.uuid.clone()),
                        bad_cycles: 0,
                    })
                } else {
                    State::Bad(BadState {
                        timestamp: bad.timestamp,
                        uuid: bad.uuid.clone(),
                        cycles: bad.cycles + 1,
                        good_cycles: bad.good_cycles + 1,
                    })
                }
            }

            State::Error(error) => {
                self.state = *error.shadowed_state.clone();
                let (shadowed_state, shadowed_trigger) =
                    self.bad_update_state(&error.shadowed_state);
                trigger = shadowed_trigger;
                shadowed_state
            }
        };
        (new_state, trigger)
    }

    async fn error(&mut self, placeholders: PlaceholderMap) -> Result<()> {
        let (new_state, trigger) = self.error_update_state(&self.state);
        self.state = new_state;
        if trigger {
            self.trigger_error(placeholders).await?;
        }
        Ok(())
    }

    async fn bad(&mut self, placeholders: PlaceholderMap) -> Result<()> {
        let (new_state, trigger) = self.bad_update_state(&self.state.clone());
        self.state = new_state;
        if trigger {
            self.trigger(placeholders).await?;
        }
        Ok(())
    }

    async fn good(&mut self, placeholders: PlaceholderMap) -> Result<()> {
        let (new_state, trigger) = self.good_update_state(&self.state.clone());
        self.state = new_state;
        if trigger {
            self.trigger_recover(placeholders).await?;
        }
        Ok(())
    }

    async fn trigger(&self, mut placeholders: PlaceholderMap) -> Result<()> {
        if let State::Bad(bad) = &self.state {
            self.add_placeholders(&mut placeholders);
            placeholders.insert(
                String::from("alarm_timestamp"),
                crate::iso8601(bad.timestamp),
            );
            placeholders.insert(String::from("alarm_uuid"), bad.uuid.clone());
            match &self.action {
                Some(action) => {
                    log::debug!("Action 'TODO' for alarm '{}' triggered.", self.name);
                    action.trigger(placeholders).await
                }
                None => {
                    log::debug!(
                        "Action for alarm '{}' was triggered but is disabled.",
                        self.name
                    );
                    Ok(())
                }
            }
        } else {
            panic!();
        }
    }

    async fn trigger_recover(&self, mut placeholders: PlaceholderMap) -> Result<()> {
        if let State::Good(good) = &self.state {
            self.add_placeholders(&mut placeholders);
            if let Some(last_alarm_uuid) = &good.last_alarm_uuid {
                placeholders.insert(String::from("alarm_uuid"), last_alarm_uuid.clone());
            }
            crate::merge_placeholders(&mut placeholders, &self.recover_placeholders);
            match &self.recover_action {
                Some(action) => action.trigger(placeholders).await,
                None => Ok(()),
            }
        } else {
            panic!();
        }
    }

    async fn trigger_error(&self, mut placeholders: PlaceholderMap) -> Result<()> {
        if let State::Error(error) = &self.state {
            self.add_placeholders(&mut placeholders);
            // TODO add info about shadowed_state (add bad uuid and timestamp, ..)
            placeholders.insert(String::from("error_uuid"), error.uuid.clone());
            placeholders.insert(
                String::from("error_timestamp"),
                crate::iso8601(error.timestamp),
            );
            crate::merge_placeholders(&mut placeholders, &self.error_placeholders);
            match &self.error_action {
                Some(action) => action.trigger(placeholders).await,
                None => Ok(()),
            }
        } else {
            panic!();
        }
    }

    fn add_placeholders(&self, placeholders: &mut PlaceholderMap) {
        placeholders.insert(String::from("alarm_name"), self.name.clone());
        crate::merge_placeholders(placeholders, &self.placeholders);
    }
}

#[async_trait]
impl<T> Alarm for AlarmBase<T>
where
    T: DataSink,
{
    type Item = T::Item;

    async fn put_data(
        &mut self,
        data: &Self::Item,
        mut placeholders: PlaceholderMap,
    ) -> Result<()> {
        log::debug!(
            "Got {} for alarm '{}' at id '{}'",
            T::format_data(data),
            self.name,
            self.id
        );
        placeholders.insert(String::from("alarm_name"), self.name.clone());
        let mut decision = self.data_sink.put_data(data)?;
        if self.invert {
            decision = !decision;
        }
        match decision {
            SinkDecision::Good => self.good(placeholders).await,
            SinkDecision::Bad => self.bad(placeholders).await,
        }
    }

    async fn put_error(&mut self, error: &Error, mut placeholders: PlaceholderMap) -> Result<()> {
        log::debug!(
            "Got error for alarm '{}' at id '{}': {}",
            self.name,
            self.id,
            error
        );
        placeholders.insert(String::from("alarm_name"), self.name.clone());
        self.error(placeholders).await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use mockall::predicate::*;

    #[tokio::test]
    async fn test_trigger_action() {
        let mut mock_data_sink = MockDataSink::new();
        let mut mock_action = action::MockAction::new();
        mock_action.expect_trigger().never();
        mock_data_sink
            .expect_put_data()
            .with(eq(10))
            .returning(|_| Ok(SinkDecision::Good));
        mock_data_sink
            .expect_put_data()
            .with(eq(20))
            .returning(|_| Ok(SinkDecision::Bad));
        let mut alarm = AlarmBase::new(
            String::from("Name"),
            String::from("ID"),
            Some(std::sync::Arc::new(mock_action)),
            PlaceholderMap::from([(String::from("Hello"), String::from("World"))]),
            5,
            0,
            None,
            PlaceholderMap::new(),
            1,
            None,
            PlaceholderMap::new(),
            0,
            false,
            mock_data_sink,
        );
        for _ in 0..4 {
            alarm.put_data(&20, PlaceholderMap::new()).await.unwrap();
        }
        let mut mock_action = action::MockAction::new();
        mock_action
            .expect_trigger()
            .once()
            .with(function(|placeholders: &PlaceholderMap| {
                uuid::Uuid::parse_str(placeholders.get("alarm_uuid").unwrap()).unwrap();
                placeholders.get("alarm_timestamp").unwrap();
                placeholders.get("alarm_name").unwrap() == "Name"
                    && placeholders.get("Hello").unwrap() == "World"
                    && placeholders.get("Foo").unwrap() == "Bar"
                    && placeholders.len() == 5
            }))
            .returning(|_| Ok(()));
        alarm.action = Some(std::sync::Arc::new(mock_action));
        alarm
            .put_data(
                &20,
                PlaceholderMap::from([(String::from("Foo"), String::from("Bar"))]),
            )
            .await
            .unwrap();
        let mut mock_action = action::MockAction::new();
        mock_action.expect_trigger().never();
        alarm.action = Some(std::sync::Arc::new(mock_action));
        for _ in 0..4 {
            alarm.put_data(&20, PlaceholderMap::new()).await.unwrap();
        }
    }
}
