//! Application-owned adapter between Gloamwire and Discord's native libdave.
//!
//! libdave owns MLS and media cryptography; this module only translates typed
//! gateway events, maintains transition state and refuses unencrypted media.

use std::collections::HashMap;

use gloamwire::{
    model::{ChannelId, UserId},
    voice::{
        DaveGatewayCommand, DaveParticipantSet, DaveProposalOperation, DaveProtocolEvent,
        DaveProvider, DaveProviderError, DaveProviderLifecycle,
    },
};
use libdave_rs::{Codec, MediaType, SessionManager, max_supported_protocol_version};

pub(crate) struct LibDaveProvider {
    manager: SessionManager,
    ssrc: Option<u32>,
    active_version: u16,
    epoch_version: u16,
    ready: bool,
    pending: HashMap<u16, u16>,
}

impl LibDaveProvider {
    pub(crate) fn new(user_id: UserId, channel_id: ChannelId) -> Result<Self, DaveProviderError> {
        let manager = SessionManager::new(user_id.get().to_string(), channel_id.get(), None)
            .map_err(backend_error)?;
        Ok(Self {
            manager,
            ssrc: None,
            active_version: 0,
            epoch_version: 0,
            ready: false,
            pending: HashMap::new(),
        })
    }

    pub(crate) fn set_ssrc(&mut self, ssrc: u32) {
        self.manager
            .encryptor_mut()
            .assign_ssrc_to_codec(ssrc, Codec::Opus);
        self.ssrc = Some(ssrc);
    }

    fn validate_version(version: u16) -> Result<(), DaveProviderError> {
        if version == 0 || version > max_supported_protocol_version() {
            return Err(DaveProviderError::new(
                "unsupported or unencrypted DAVE protocol version",
            ));
        }
        Ok(())
    }

    fn key_package(&self) -> Result<Vec<DaveGatewayCommand>, DaveProviderError> {
        let package = self.manager.marshalled_key_package();
        if package.is_empty() {
            return Err(DaveProviderError::new("libdave produced no MLS key package"));
        }
        Ok(vec![DaveGatewayCommand::KeyPackage(package)])
    }

    fn refresh_ready(&mut self) {
        let encryptor = self.manager.encryptor_mut();
        self.ready = self.active_version != 0
            && self.ssrc.is_some()
            && encryptor.has_key_ratchet()
            && !encryptor.is_passthrough_mode();
    }

    fn prepare_ready(
        &mut self,
        transition_id: u16,
        version: u16,
        acknowledge: bool,
    ) -> Result<Vec<DaveGatewayCommand>, DaveProviderError> {
        Self::validate_version(version)?;
        if transition_id == 0 {
            self.active_version = version;
            self.refresh_ready();
            if !self.ready {
                return Err(DaveProviderError::new(
                    "DAVE transition completed without an encryption ratchet",
                ));
            }
            return Ok(Vec::new());
        }
        if !acknowledge {
            return Err(DaveProviderError::new("libdave did not prepare DAVE transition"));
        }
        self.pending.insert(transition_id, version);
        // Keep the previous active ratchet only until the gateway executes the transition.
        Ok(vec![DaveGatewayCommand::ReadyForTransition { transition_id }])
    }
}

impl DaveProviderLifecycle for LibDaveProvider {
    fn configure_protocol_version(
        &mut self,
        version: u16,
    ) -> Result<Vec<DaveGatewayCommand>, DaveProviderError> {
        Self::validate_version(version)?;
        self.ready = false;
        self.pending.clear();
        self.manager
            .on_select_protocol_ack(version)
            .map_err(backend_error)?;
        self.active_version = version;
        self.epoch_version = version;
        self.key_package()
    }

    fn active_protocol_version(&self) -> u16 {
        self.active_version
    }

    fn is_ready(&self) -> bool {
        self.ready
    }
}

impl DaveProvider for LibDaveProvider {
    fn max_protocol_version(&self) -> u16 {
        max_supported_protocol_version()
    }

    fn handle_gateway_event(
        &mut self,
        event: &DaveProtocolEvent,
        _participants: &DaveParticipantSet,
    ) -> Result<Vec<DaveGatewayCommand>, DaveProviderError> {
        match event {
            DaveProtocolEvent::ClientsConnect { user_ids } => {
                for user_id in user_ids {
                    self.manager
                        .add_user(&user_id.get().to_string())
                        .map_err(backend_error)?;
                }
            }
            DaveProtocolEvent::ClientDisconnect { user_id } => {
                self.manager
                    .remove_user(&user_id.get().to_string())
                    .map_err(backend_error)?;
            }
            DaveProtocolEvent::PrepareTransition {
                protocol_version,
                transition_id,
            } => {
                Self::validate_version(*protocol_version)?;
                let acknowledge = self
                    .manager
                    .on_prepare_transition(*transition_id, *protocol_version)
                    .map_err(backend_error)?;
                return self.prepare_ready(
                    *transition_id,
                    *protocol_version,
                    acknowledge.is_some(),
                );
            }
            DaveProtocolEvent::ExecuteTransition { transition_id } => {
                let version = self.pending.remove(transition_id).ok_or_else(|| {
                    DaveProviderError::new("unprepared DAVE execute transition")
                })?;
                self.manager
                    .on_execute_transition(*transition_id)
                    .map_err(backend_error)?;
                self.active_version = version;
                self.refresh_ready();
                if !self.ready {
                    return Err(DaveProviderError::new("DAVE media ratchet unavailable"));
                }
            }
            DaveProtocolEvent::PrepareEpoch {
                protocol_version,
                epoch,
            } => {
                Self::validate_version(*protocol_version)?;
                if *epoch == 1 {
                    self.epoch_version = *protocol_version;
                    self.ready = false;
                    self.pending.clear();
                    self.manager
                        .on_prepare_epoch(*epoch, *protocol_version)
                        .map_err(backend_error)?;
                    return self.key_package();
                }
            }
            DaveProtocolEvent::ExternalSender { package } => {
                self.manager.on_external_sender_package(package);
            }
            DaveProtocolEvent::Proposals { operation, payload } => {
                // The native API takes the operation byte followed by MLS proposals.
                // Gloamwire has already split that byte into a typed enum.
                let operation = match operation {
                    DaveProposalOperation::Append => 0,
                    DaveProposalOperation::Revoke => 1,
                    DaveProposalOperation::Unknown(_) => {
                        return Err(DaveProviderError::new("unknown MLS proposal operation"));
                    }
                };
                let mut wire = Vec::with_capacity(payload.len() + 1);
                wire.push(operation);
                wire.extend_from_slice(payload);
                if let Some(commit_welcome) = self.manager.on_mls_proposals(&wire) {
                    if !commit_welcome.is_empty() {
                        return Ok(vec![DaveGatewayCommand::CommitWelcome(commit_welcome)]);
                    }
                }
            }
            DaveProtocolEvent::AnnounceCommit {
                transition_id,
                commit,
            } => {
                let result = self
                    .manager
                    .on_mls_commit(*transition_id, commit)
                    .map_err(backend_error)?;
                if result.failed {
                    self.ready = false;
                    return Err(DaveProviderError::new(
                        "libdave rejected MLS commit; reconnect required",
                    ));
                }
                return self.prepare_ready(*transition_id, self.epoch_version, result.ready);
            }
            DaveProtocolEvent::Welcome {
                transition_id,
                welcome,
            } => {
                let result = self
                    .manager
                    .on_mls_welcome(*transition_id, welcome)
                    .map_err(backend_error)?;
                return self.prepare_ready(*transition_id, self.epoch_version, result.ready);
            }
            DaveProtocolEvent::Unknown(_) => {}
        }
        Ok(Vec::new())
    }

    fn encrypt_opus(&mut self, frame: &[u8]) -> Result<Vec<u8>, DaveProviderError> {
        if !self.ready || self.manager.encryptor_mut().is_passthrough_mode() {
            return Err(DaveProviderError::new("DAVE encryption not ready"));
        }
        let ssrc = self
            .ssrc
            .ok_or_else(|| DaveProviderError::new("DAVE Opus SSRC not configured"))?;
        self.manager
            .encrypt(MediaType::Audio, ssrc, frame)
            .map_err(backend_error)
    }

    fn decrypt_opus(&mut self, sender: UserId, frame: &[u8]) -> Result<Vec<u8>, DaveProviderError> {
        if !self.ready {
            return Err(DaveProviderError::new("DAVE decryption not ready"));
        }
        self.manager
            .decrypt(&sender.get().to_string(), MediaType::Audio, frame)
            .map_err(backend_error)
    }
}

fn backend_error(error: impl std::fmt::Display) -> DaveProviderError {
    DaveProviderError::new(format!("libdave: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_plaintext_and_uninitialized_media() {
        let mut provider = LibDaveProvider::new(UserId::new(10), ChannelId::new(20)).unwrap();
        assert!(provider.configure_protocol_version(0).is_err());
        assert!(provider.encrypt_opus(&[0xf8, 0xff, 0xfe]).is_err());
        assert!(!provider.is_ready());
    }

    #[test]
    fn negotiates_dave_and_generates_key_package() {
        let mut provider = LibDaveProvider::new(UserId::new(10), ChannelId::new(20)).unwrap();
        provider.set_ssrc(123);
        let commands = provider.configure_protocol_version(1).unwrap();
        assert!(
            matches!(&commands[..], [DaveGatewayCommand::KeyPackage(bytes)] if !bytes.is_empty())
        );
        assert_eq!(provider.active_protocol_version(), 1);
        assert!(!provider.is_ready());
    }
}
