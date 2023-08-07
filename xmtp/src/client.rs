use core::fmt;
use std::fmt::Formatter;

use diesel::Connection;
use log::{debug, info};
use thiserror::Error;
use vodozemac::olm::OlmMessage;

use crate::{
    account::Account,
    contact::{Contact, ContactError},
    session::SessionManager,
    storage::{
        now, DbConnection, EncryptedMessageStore, StorageError, StoredInstallation, StoredSession,
    },
    types::networking::{PublishRequest, QueryRequest, XmtpApiClient},
    types::Address,
    utils::{build_envelope, build_user_contact_topic, key_fingerprint},
    Store,
};
use std::collections::HashMap;
use xmtp_proto::xmtp::message_api::v1::Envelope;

#[derive(Clone, Copy, Default, Debug)]
pub enum Network {
    Local(&'static str),
    #[default]
    Dev,
    Prod,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("contact error {0}")]
    Contact(#[from] ContactError),
    #[error("could not publish: {0}")]
    PublishError(String),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("dieselError: {0}")]
    Ddd(#[from] diesel::result::Error),
    #[error("Query failed: {0}")]
    QueryError(String),
    #[error("unknown client error")]
    Unknown,
}

pub struct Client<A>
where
    A: XmtpApiClient,
{
    pub api_client: A,
    pub(crate) network: Network,
    pub(crate) account: Account,
    pub store: EncryptedMessageStore, // Temporarily exposed outside crate for CLI client
    is_initialized: bool,
}

impl<A> core::fmt::Debug for Client<A>
where
    A: XmtpApiClient,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Client({:?})::{}", self.network, self.account.addr())
    }
}

impl<A> Client<A>
where
    A: XmtpApiClient,
{
    pub fn new(
        api_client: A,
        network: Network,
        account: Account,
        store: EncryptedMessageStore,
    ) -> Self {
        Self {
            api_client,
            network,
            account,
            store,
            is_initialized: false,
        }
    }

    pub fn wallet_address(&self) -> Address {
        self.account.addr()
    }

    pub async fn init(&mut self) -> Result<(), ClientError> {
        let app_contact_bundle = self.account.contact();
        let registered_bundles = self.get_contacts(&self.wallet_address()).await?;

        if !registered_bundles
            .iter()
            .any(|contact| contact.installation_id() == app_contact_bundle.installation_id())
        {
            self.publish_user_contact().await?;
        }

        self.refresh_user_installations(&app_contact_bundle.wallet_address)
            .await?;

        self.is_initialized = true;
        Ok(())
    }

    pub async fn get_contacts(&self, wallet_address: &str) -> Result<Vec<Contact>, ClientError> {
        let topic = build_user_contact_topic(wallet_address.to_string());
        let response = self
            .api_client
            .query(QueryRequest {
                content_topics: vec![topic],
                start_time_ns: 0,
                end_time_ns: 0,
                paging_info: None,
            })
            .await
            .map_err(|e| ClientError::QueryError(format!("Could not query for contacts: {}", e)))?;

        let mut contacts = vec![];
        for envelope in response.envelopes {
            let contact_bundle = Contact::from_bytes(envelope.message, wallet_address.to_string());
            match contact_bundle {
                Ok(bundle) => {
                    contacts.push(bundle);
                }
                Err(err) => {
                    println!("bad contact bundle: {:?}", err);
                }
            }
        }

        Ok(contacts)
    }

    pub fn get_session(
        &self,
        contact: &Contact,
        conn: &mut DbConnection,
    ) -> Result<SessionManager, ClientError> {
        let existing_session = self
            .store
            .get_session_with_conn(&contact.installation_id(), conn)?;
        match existing_session {
            Some(i) => Ok(SessionManager::try_from(&i)?),
            None => self.create_outbound_session(contact),
        }
    }

    pub async fn my_other_devices(&self) -> Result<Vec<Contact>, ClientError> {
        let contacts = self.get_contacts(self.account.addr().as_str()).await?;
        let my_contact_id = self.account.contact().installation_id();
        Ok(contacts
            .into_iter()
            .filter(|c| c.installation_id() != my_contact_id)
            .collect())
    }

    /// Fetch Installations from the Network and create unintialized sessions for newly discovered contacts
    // TODO: Reduce Visibility
    pub async fn refresh_user_installations(&self, user_address: &str) -> Result<(), ClientError> {
        // Store the timestamp of when the refresh process begins
        let refresh_timestmap = now();

        let self_install_id = key_fingerprint(&self.account.identity_keys().curve25519);
        let contacts = self.get_contacts(user_address).await?;
        debug!(
            "Fetched contacts for address {}: {:?}",
            user_address, contacts
        );

        let installation_map = self
            .store
            .get_installations(user_address)?
            .into_iter()
            .map(|v| (v.installation_id.clone(), v))
            .collect::<HashMap<_, _>>();

        let new_installs: Vec<StoredInstallation> = contacts
            .iter()
            .filter(|contact| self_install_id != contact.installation_id())
            .filter(|contact| !installation_map.contains_key(&contact.installation_id()))
            .filter_map(|contact| StoredInstallation::new(contact).ok())
            .collect();
        debug!(
            "New installs for address {}: {:?}",
            user_address, new_installs
        );

        self.store.conn().unwrap().transaction(
            |transaction_manager| -> Result<(), ClientError> {
                for install in new_installs {
                    info!("Saving Install {}", install.installation_id);
                    let session = self.create_uninitialized_session(&install.get_contact()?)?;
                    self.store
                        .insert_or_ignore_install(install, transaction_manager)?;
                    self.store.insert_or_ignore_session(
                        StoredSession::try_from(&session)?,
                        transaction_manager,
                    )?;
                }

                self.store.update_user_refresh_timestamp(
                    transaction_manager,
                    user_address,
                    refresh_timestmap,
                )?;

                Ok(())
            },
        )?;

        Ok(())
    }

    pub fn create_uninitialized_session(
        &self,
        contact: &Contact,
    ) -> Result<SessionManager, ClientError> {
        let olm_session = self.account.create_outbound_session(contact);
        let session = SessionManager::from_olm_session(olm_session, contact)
            .map_err(|_| ClientError::Unknown)?;

        Ok(session)
    }

    pub fn create_outbound_session(
        &self,
        contact: &Contact,
    ) -> Result<SessionManager, ClientError> {
        let olm_session = self.account.create_outbound_session(contact);
        let session = SessionManager::from_olm_session(olm_session, contact)
            .map_err(|_| ClientError::Unknown)?;

        session.store(&mut self.store.conn().unwrap())?;

        Ok(session)
    }

    pub fn create_inbound_session(
        &self,
        conn: &mut DbConnection,
        contact: &Contact,
        // Message MUST be a pre-key message
        message: &Vec<u8>,
    ) -> Result<(SessionManager, Vec<u8>), ClientError> {
        let olm_message: OlmMessage =
            serde_json::from_slice(message.as_slice()).map_err(|_| ClientError::Unknown)?;
        let msg = match olm_message {
            OlmMessage::PreKey(msg) => msg,
            _ => return Err(ClientError::Unknown),
        };

        let create_result = self
            .account
            .create_inbound_session(contact, msg)
            .map_err(|_| ClientError::Unknown)?;

        let session = SessionManager::from_olm_session(create_result.session, contact)
            .map_err(|_| ClientError::Unknown)?;

        session.store(conn)?;

        Ok((session, create_result.plaintext))
    }

    async fn publish_user_contact(&self) -> Result<(), ClientError> {
        let envelope = self.build_contact_envelope()?;
        self.api_client
            .publish(
                "".to_string(),
                PublishRequest {
                    envelopes: vec![envelope],
                },
            )
            .await
            .map_err(|e| ClientError::PublishError(format!("Could not publish contact: {}", e)))?;

        Ok(())
    }

    fn build_contact_envelope(&self) -> Result<Envelope, ClientError> {
        let contact = self.account.contact();

        let envelope = build_envelope(
            build_user_contact_topic(self.wallet_address()),
            contact.try_into()?,
        );

        Ok(envelope)
    }

    pub async fn download_latest_from_topic(
        &self,
        start_time: u64,
        topic: String,
    ) -> Result<Vec<Envelope>, ClientError> {
        let response = self
            .api_client
            .query(QueryRequest {
                content_topics: vec![topic],
                start_time_ns: start_time,
                end_time_ns: 0,
                // TODO: Pagination
                paging_info: None,
            })
            .await
            .map_err(|e| ClientError::QueryError(format!("Could not query topic: {}", e)))?;

        Ok(response.envelopes)
    }
}

#[cfg(test)]
mod tests {
    use xmtp_proto::xmtp::v3::message_contents::installation_contact_bundle::Version;
    use xmtp_proto::xmtp::v3::message_contents::vmac_unsigned_public_key::Union::Curve25519;
    use xmtp_proto::xmtp::v3::message_contents::vmac_unsigned_public_key::VodozemacCurve25519;

    use crate::test_utils::test_utils::gen_test_client;
    use crate::ClientBuilder;

    #[tokio::test]
    async fn registration() {
        gen_test_client().await;
    }

    #[tokio::test]
    async fn refresh() {
        let client = ClientBuilder::new_test().build().unwrap();
        client
            .refresh_user_installations(&client.wallet_address())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_publish_user_contact() {
        let client = ClientBuilder::new_test().build().unwrap();
        client
            .publish_user_contact()
            .await
            .expect("Failed to publish user contact");

        let contacts = client
            .get_contacts(client.wallet_address().as_str())
            .await
            .unwrap();

        assert_eq!(contacts.len(), 1);
        let installation_bundle = match contacts[0].clone().bundle.version.unwrap() {
            Version::V1(bundle) => bundle,
        };
        assert!(installation_bundle.fallback_key.is_some());
        assert!(installation_bundle.identity_key.is_some());
        contacts[0].vmac_identity_key();
        contacts[0].vmac_fallback_key();

        let key_bytes = installation_bundle
            .clone()
            .identity_key
            .unwrap()
            .key
            .unwrap()
            .union
            .unwrap();

        match key_bytes {
            Curve25519(VodozemacCurve25519 { bytes }) => {
                assert_eq!(bytes.len(), 32);
                assert_eq!(
                    client
                        .account
                        .olm_account()
                        .unwrap()
                        .get()
                        .curve25519_key()
                        .to_bytes()
                        .to_vec(),
                    bytes
                )
            }
        }
    }
}
