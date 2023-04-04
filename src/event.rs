use std::{
    fmt::Display,
    future::Future,
    hash::Hash,
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Duration,
};

use redis::Commands;
use tokio::sync::watch;
use typed_builder::TypedBuilder;

use crate::app::AppData;

#[derive(Debug, Default, Clone, Copy)]
pub struct State<S>(pub S);

impl<S> Deref for State<S> {
    type Target = S;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S> DerefMut for State<S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(TypedBuilder)]
pub struct EventWatcher<S> {
    #[builder(setter( transform = |s: impl Display| Arc::new(s.to_string().into()) ))]
    name: Arc<Box<str>>,
    #[builder(default = 60)]
    heartbeat_interval: u64,
    pub bot: teloxide::Bot,
    pub data: AppData,
    #[builder(default, setter( transform = |s: S| Some(Arc::new(State(s))) ))]
    pub state: Option<Arc<State<S>>>,
}

impl<S> Clone for EventWatcher<S> {
    fn clone(&self) -> Self {
        // bot & data is already wrapped by Arc
        Self {
            name: Arc::clone(&self.name),
            heartbeat_interval: self.heartbeat_interval,
            bot: self.bot.clone(),
            data: self.data.clone(),
            state: self.state.clone(),
        }
    }
}

pub trait Promise: Future<Output = anyhow::Result<()>> + Send + 'static {}
impl<T> Promise for T where T: Future<Output = anyhow::Result<()>> + Send + 'static {}

impl<S> EventWatcher<S> {
    pub fn start<P, T>(self, task: T)
    where
        P: Promise,
        S: Send + Sync + 'static,
        T: Fn(EventWatcher<S>) -> P + Sync + Send + 'static,
    {
        let (tx, rx) = watch::channel(1_u8);
        let mut heartbeat = tokio::time::interval(Duration::from_secs(self.heartbeat_interval));
        let name = self.name.to_string();

        tokio::spawn(async move {
            loop {
                let watcher = self.clone();
                let mut rx = rx.clone();

                tokio::select! {
                    _ = rx.changed() => {
                        break;
                    }
                    _ = heartbeat.tick() => {
                        if let Err(err) = task(watcher).await {
                            tracing::error!("{}", err)
                        }
                    }
                }
            }
        });

        let quit_on_ctrl_c = || async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Quiting event watcher for {}...", name);
            tx.send(0)
                .unwrap_or_else(|_| panic!("fail to send signal into event watcher {}", name));
        };

        tokio::spawn(quit_on_ctrl_c());
    }

    // Create `event = [registrant]` key-value pair
    pub fn subscribe_event<Subscriber, Event>(
        &self,
        registrant: &Subscriber,
        events: &Vec<Event>,
    ) -> anyhow::Result<()>
    where
        Subscriber: redis::ToRedisArgs,
        Event: redis::ToRedisArgs + std::fmt::Display,
    {
        let mut conn = self.data.cacher.get_conn();
        let event_pool_key = format!("REGISTRY_EVENT_POOL:{}", self.name);
        for event in events {
            let key = format!("SUBSCRIBE_REGISTRY:{}:{}", self.name, event);
            conn.rpush(key, registrant)?;
            conn.sadd(event_pool_key.as_str(), event)?;
        }

        Ok(())
    }

    pub fn setup_subscribe_registry<'iter, Subscriber, Event, Relation>(
        self,
        iter: Relation,
    ) -> Self
    where
        Subscriber: Eq + Hash + std::fmt::Debug + redis::ToRedisArgs + 'iter,
        Event: Eq + Hash + std::fmt::Debug + std::fmt::Display + redis::ToRedisArgs + 'iter,
        Relation: Iterator<Item = (&'iter Subscriber, &'iter Vec<Event>)>,
    {
        iter.for_each(|(k, v)| {
            self.subscribe_event(k, v).unwrap_or_else(|err| {
                panic!(
                    "fail to initialize the {} subscribe registry \
                        when subscribe event {:?} for registrant {:?}: \
                        {err}",
                    self.name, v, k
                )
            });
        });

        self
    }

    pub fn event_pool<Event>(&self) -> anyhow::Result<Vec<Event>>
    where
        Event: redis::FromRedisValue,
    {
        let event_pool_key = format!("REGISTRY_EVENT_POOL:{}", self.name);
        let events = self.data.cacher.get_conn().smembers(event_pool_key)?;
        Ok(events)
    }

    pub fn get_subscribers<Subscriber, Event>(
        &self,
        event: &Event,
    ) -> anyhow::Result<Vec<Subscriber>>
    where
        Subscriber: redis::FromRedisValue,
        Event: redis::ToRedisArgs + std::fmt::Display,
    {
        let key = format!("SUBSCRIBE_REGISTRY:{}:{}", self.name, event);
        let subscriber = self.data.cacher.get_conn().lrange(key, 0, -1)?;
        Ok(subscriber)
    }
}
