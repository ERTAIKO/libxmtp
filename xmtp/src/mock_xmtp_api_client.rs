use crate::types::networking::*;
use async_trait::async_trait;
use std::collections::HashMap;

pub struct MockXmtpApiSubscription {}

impl XmtpApiSubscription for MockXmtpApiSubscription {
    fn is_closed(&self) -> bool {
        false
    }

    fn get_messages(&self) -> Vec<Envelope> {
        vec![]
    }

    fn close_stream(&mut self) {}
}

pub struct MockXmtpApiClient {
    messages: std::sync::Mutex<HashMap<String, Vec<Envelope>>>,
    app_version: String,   
}

impl MockXmtpApiClient {
    pub fn new() -> Self {
        Self {
            messages: std::sync::Mutex::new(HashMap::new()),
            app_version: String::from("0.0.0"),
        }
    }
}

impl Default for MockXmtpApiClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl XmtpApiClient for MockXmtpApiClient {
    type Subscription = MockXmtpApiSubscription;

    fn set_app_version(&mut self, version: String) {
        self.app_version = version;
    }

    async fn publish(
        &self,
        _token: String,
        request: PublishRequest,
    ) -> Result<PublishResponse, Error> {
        let mut messages = self.messages.lock().unwrap();
        for envelope in request.envelopes {
            let topic = envelope.content_topic.clone();
            let mut existing: Vec<Envelope> = match messages.get(&topic) {
                Some(existing_envelopes) => existing_envelopes.clone(),
                None => vec![],
            };
            existing.push(envelope);
            messages.insert(topic, existing);
        }
        Ok(PublishResponse {})
    }

    async fn query(&self, request: QueryRequest) -> Result<QueryResponse, Error> {
        let messages = self.messages.lock().unwrap();
        let envelopes: Vec<Envelope> = match messages.get(&request.content_topics[0]) {
            Some(envelopes) => envelopes.clone(),
            None => vec![],
        };

        Ok(QueryResponse {
            envelopes,
            paging_info: None,
        })
    }

    async fn subscribe(&self, _request: SubscribeRequest) -> Result<Self::Subscription, Error> {
        Err(Error::new(ErrorKind::SubscribeError))
    }
}