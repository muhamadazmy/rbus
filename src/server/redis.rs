use super::Object;
use super::{Error, Result};
use crate::protocol::{Output, Request, Response};
use bb8_redis::{bb8::Pool, RedisConnectionManager};
use redis::AsyncCommands;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

const PULL_TIMEOUT: usize = 10;
const RESPONSE_TTL: usize = 5 * 60;

type Routers = HashMap<String, Box<dyn Object + Send + Sync>>;

pub struct Server {
    module: String,
    pool: Pool<RedisConnectionManager>,
    workers: usize,
    objects: Routers,
}

impl Server {
    pub async fn new<S>(
        pool: Pool<RedisConnectionManager>,
        module: S,
        workers: usize,
    ) -> Result<Server>
    where
        S: AsRef<str>,
    {
        assert!(workers >= 1, "workers must be at least 1");

        Ok(Server {
            pool,
            workers,
            module: module.as_ref().into(),
            objects: Routers::new(),
        })
    }

    pub fn register<T>(&mut self, service: T)
    where
        T: Object + Send + Sync + 'static,
    {
        self.objects
            .insert(service.id().to_string(), Box::new(service));
    }

    pub async fn run(self) {
        // routers can not be changed afterwords. so we need to spawn workers here
        // and pass them a copy of the routers, and a way for them to pull for messages.

        let module = self.module;
        let routers = self.objects;
        let queues: Vec<String> = routers
            .keys()
            .map(|k| format!("{}.{}", module, k))
            .collect();

        log::debug!("pulling from: {:?}", queues);
        let worker = Worker::new(self.pool.clone(), routers);
        let mut workers = workers::WorkerPool::new(worker, self.workers);

        loop {
            let worker = workers.get().await;

            loop {
                let mut con = match self.pool.get().await {
                    Ok(con) => con,
                    Err(err) => {
                        log::error!("failed to get redis connection: {}", err);
                        sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };

                let (_, request): (String, Request) = match con.blpop(&queues, PULL_TIMEOUT).await {
                    Err(err) => {
                        log::error!("failed to get get request: {}", err);
                        sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    Ok(Some(value)) => value,
                    Ok(None) => continue,
                };

                if let Err(err) = worker.send(request) {
                    log::error!("failed to schedule request: {}", err);
                }

                break;
            }
        }
    }
}

#[derive(Clone)]
struct Worker {
    routers: Arc<Routers>,
    pool: Pool<RedisConnectionManager>,
}

impl Worker {
    fn new(pool: Pool<RedisConnectionManager>, routers: Routers) -> Self {
        Self {
            pool,
            routers: Arc::new(routers),
        }
    }

    async fn respond<S: Into<String>>(&self, id: S, ret: Result<Output>) -> anyhow::Result<()> {
        use anyhow::Context;

        let id = id.into();

        let response = match ret {
            Ok(output) => Response {
                id: id.clone(),
                output: output,
                error: None,
            },
            Err(err) => Response {
                id: id.clone(),
                output: Output::default(),
                error: Some(err.to_string()),
            },
        };

        let mut con = self
            .pool
            .get()
            .await
            .context("failed to get redis connection")?;

        con.rpush(&id, response)
            .await
            .context("failed to push response")?;
        let _ = con.expire::<_, ()>(&id, RESPONSE_TTL).await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl workers::Work for Worker {
    type Input = Request;
    type Output = ();

    async fn run(&self, input: Self::Input) -> Self::Output {
        // dispatch message to handlers.
        let id = input.id.clone();
        let object = input.object.to_string();
        let response = match self.routers.get(&object) {
            Some(service) => service.dispatch(input).await,
            None => Err(Error::UnknownObject(object.clone())),
        };

        if let Err(err) = self.respond(id, response).await {
            log::error!("failed to send response: {}", err);
        }
    }
}
