/*
 * Copyright 2018 Bitwise IO, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 * -----------------------------------------------------------------------------
 */

//! The core PBFT algorithm

use std::collections::HashSet;
use std::convert::From;
use std::error::Error;

use hex;
use protobuf::{Message, RepeatedField};
use sawtooth_sdk::consensus::engine::{Block, BlockId, Error as EngineError, PeerId};
use sawtooth_sdk::consensus::service::Service;
use sawtooth_sdk::messages::consensus::ConsensusPeerMessageHeader;
use sawtooth_sdk::signing::{create_context, secp256k1::Secp256k1PublicKey};

use crate::config::{get_peers_from_settings, PbftConfig};
use crate::error::PbftError;
use crate::hash::verify_sha512;
use crate::message_log::PbftLog;
use crate::message_type::{ParsedMessage, PbftMessageType};
use crate::protos::pbft_message::{
    PbftBlock, PbftMessage, PbftMessageInfo, PbftNewView, PbftSeal, PbftSignedVote,
};
use crate::state::{PbftMode, PbftPhase, PbftState};
use crate::timing::Timeout;

/// Contains the core logic of the PBFT node
pub struct PbftNode {
    /// Used for interactions with the validator
    pub service: Box<Service>,

    /// Log of messages this node has received and accepted
    pub msg_log: PbftLog,
}

impl PbftNode {
    /// Construct a new PBFT node
    ///
    /// If the node is the primary on start-up, it initializes a new block on the chain
    pub fn new(config: &PbftConfig, service: Box<Service>, is_primary: bool) -> Self {
        let mut n = PbftNode {
            service,
            msg_log: PbftLog::new(config),
        };

        // Primary initializes a block
        if is_primary {
            n.service
                .initialize_block(None)
                .unwrap_or_else(|err| error!("Couldn't initialize block: {}", err));
        }
        n
    }

    // ---------- Methods for handling Updates from the Validator ----------

    /// Handle a peer message from another PbftNode
    ///
    /// Handle all messages from other nodes. Such messages include `PrePrepare`, `Prepare`,
    /// `Commit`, `ViewChange`, and `NewView`. If the node is view changing, ignore all messages
    /// that aren't `ViewChange`s or `NewView`s.
    pub fn on_peer_message(
        &mut self,
        msg: ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        info!("{}: Got peer message: {}", state, msg.info());

        let msg_type = PbftMessageType::from(msg.info().msg_type.as_str());

        // If this node is in the process of a view change, ignore all messages except ViewChanges
        // and NewViews
        if match state.mode {
            PbftMode::ViewChanging(_) => true,
            _ => false,
        } && msg_type != PbftMessageType::ViewChange
            && msg_type != PbftMessageType::NewView
        {
            warn!(
                "{}: Node is view changing; ignoring {} message",
                state, msg_type
            );
            return Ok(());
        }

        match msg_type {
            PbftMessageType::PrePrepare => self.handle_pre_prepare(msg, state)?,
            PbftMessageType::Prepare => self.handle_prepare(msg, state)?,
            PbftMessageType::Commit => self.handle_commit(msg, state)?,
            PbftMessageType::ViewChange => self.handle_view_change(&msg, state)?,
            PbftMessageType::NewView => self.handle_new_view(&msg, state)?,
            _ => warn!("Message type not implemented"),
        }

        Ok(())
    }

    /// Handle a `PrePrepare` message
    ///
    /// A `PrePrepare` message is accepted and added to the log if the following are true:
    /// - The message signature is valid (already verified by validator)
    /// - The message is from the primary
    /// - There is a matching BlockNew message
    /// - A `PrePrepare` message does not already exist at this view and sequence number with a
    ///   different block
    /// - The message's view matches the node's current view (handled by message log)
    ///
    /// Once a `PrePrepare` for the current sequence number is accepted and added to the log, the
    /// node node will instruct the validator to validate the block
    fn handle_pre_prepare(
        &mut self,
        msg: ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // Check that the message is from the current primary
        if PeerId::from(msg.info().get_signer_id()) != state.get_primary_id() {
            warn!(
                "Got PrePrepare from a secondary node {:?}; ignoring message",
                msg.info().get_signer_id()
            );
            return Ok(());
        }

        // Check that there is a matching BlockNew message; if not, add the PrePrepare to the
        // backlog because we can't perform consensus until the validator has this block
        let block_new_exists = self
            .msg_log
            .get_messages_of_type_seq(PbftMessageType::BlockNew, msg.info().get_seq_num())
            .iter()
            .any(|block_new_msg| block_new_msg.get_block() == msg.get_block());
        if !block_new_exists {
            warn!(
                "No matching BlockNew found for PrePrepare {:?}; pushing to backlog",
                msg
            );
            self.msg_log.push_backlog(msg);
            return Ok(());
        }

        // Check that no `PrePrepare`s already exist with this view and sequence number but a
        // different block; if this is violated, the primary is faulty so initiate a view change
        let mut mismatched_blocks = self
            .msg_log
            .get_messages_of_type_seq_view(
                PbftMessageType::PrePrepare,
                msg.info().get_seq_num(),
                msg.info().get_view(),
            )
            .iter()
            .filter_map(|existing_msg| {
                let block = existing_msg.get_block().clone();
                if &block != msg.get_block() {
                    Some(block)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if !mismatched_blocks.is_empty() {
            warn!("When checking PrePrepare {:?}, found PrePrepare(s) with same view and seq num but mismatched block(s): {:?}", msg, mismatched_blocks);
            mismatched_blocks.push(msg.get_block().clone());
            for block in mismatched_blocks {
                self.service
                    .fail_block(block.get_block_id().to_vec())
                    .map_err(|err| {
                        PbftError::InternalError(format!("Couldn't fail block: {}", err))
                    })?;
            }
            self.propose_view_change(state, state.view + 1)?;
            return Ok(());
        }

        // Add message to the log
        self.msg_log.add_message(msg.clone(), state)?;

        // If this message is for the current sequence number and the node is in the PrePreparing
        // phase, check the block
        if msg.info().get_seq_num() == state.seq_num && state.phase == PbftPhase::PrePreparing {
            state.switch_phase(PbftPhase::Checking);

            // We can also stop the view change timer, since we received a new block and a
            // valid PrePrepare in time
            state.faulty_primary_timeout.stop();

            self.service
                .check_blocks(vec![msg.get_block().clone().block_id])
                .map_err(|_| PbftError::InternalError(String::from("Failed to check blocks")))?
        }

        Ok(())
    }

    /// Handle a `Prepare` message
    ///
    /// Once a `Prepare` for the current sequence number is accepted and added to the log, the node
    /// will check if it has the required 2f + 1 `Prepared` messages to move on to the Committing
    /// phase
    fn handle_prepare(
        &mut self,
        msg: ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let info = msg.info().clone();
        let block = msg.get_block().clone();

        self.msg_log.add_message(msg, state)?;

        // If this message is for the current sequence number and the node is in the Preparing
        // phase, check if the node is ready to move on to the Committing phase
        if info.get_seq_num() == state.seq_num && state.phase == PbftPhase::Preparing {
            // The node is ready to move on to the Committing phase (i.e. the predicate `prepared`
            // is true) when its log has 2f + 1 Prepare messages from different nodes that match
            // the PrePrepare message received earlier (same view, sequence number, and block)
            if let Some(pre_prep) = self.msg_log.get_one_msg(&info, PbftMessageType::PrePrepare) {
                if self.msg_log.log_has_required_msgs(
                    PbftMessageType::Prepare,
                    &pre_prep,
                    true,
                    2 * state.f + 1,
                ) {
                    state.switch_phase(PbftPhase::Committing);
                    self._broadcast_pbft_message(
                        state.seq_num,
                        PbftMessageType::Commit,
                        block,
                        state,
                    )?;
                }
            }
        }

        Ok(())
    }

    /// Handle a `Commit` message
    ///
    /// Once a `Commit` for the current sequence number is accepted and added to the log, the node
    /// will check if it has the required 2f + 1 `Commit` messages to actually commit the block
    fn handle_commit(
        &mut self,
        msg: ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let info = msg.info().clone();
        let block = msg.get_block().clone();

        self.msg_log.add_message(msg, state)?;

        // If this message is for the current sequence number and the node is in the Committing
        // phase, check if the node is ready to commit the block
        if info.get_seq_num() == state.seq_num && state.phase == PbftPhase::Committing {
            // The node is ready to commit the block (i.e. the predicate `committable` is true)
            // when its log has 2f + 1 Commit messages from different nodes that match the
            // PrePrepare message received earlier (same view, sequence number, and block)
            if let Some(pre_prep) = self.msg_log.get_one_msg(&info, PbftMessageType::PrePrepare) {
                if self.msg_log.log_has_required_msgs(
                    PbftMessageType::Commit,
                    &pre_prep,
                    true,
                    2 * state.f + 1,
                ) {
                    self.service
                        .commit_block(block.block_id.clone())
                        .map_err(|e| {
                            PbftError::InternalError(format!("Failed to commit block: {:?}", e))
                        })?;
                    state.switch_phase(PbftPhase::Finished);
                }
            }
        }

        Ok(())
    }

    /// Handle a `ViewChange` message
    ///
    /// When a `ViewChange` is received, check that it isn't outdated and add it to the log. If the
    /// node isn't already view changing but it now has f + 1 ViewChange messages, start view
    /// changing early. If the node is the primary and has 2f view change messages now, broadcast
    /// the NewView message to the rest of the nodes to move to the new view.
    fn handle_view_change(
        &mut self,
        msg: &ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // Ignore old view change messages (already on a view >= the one this message is
        // for or already trying to change to a later view)
        let msg_view = msg.info().get_view();
        if msg_view <= state.view
            || match state.mode {
                PbftMode::ViewChanging(v) => msg_view < v,
                _ => false,
            }
        {
            return Ok(());
        }

        self.msg_log.add_message(msg.clone(), state)?;

        // Even if we haven't detected a faulty primary yet, start view changing if we've
        // received f + 1 ViewChange messages for this proposed view (but if we're already
        // view changing, only do this for a later view); this will prevent starting the
        // view change too late
        if match state.mode {
            PbftMode::ViewChanging(v) => msg_view > v,
            PbftMode::Normal => true,
        } && self.msg_log.log_has_required_msgs(
            PbftMessageType::ViewChange,
            msg,
            false,
            state.f + 1,
        ) {
            self.propose_view_change(state, msg_view)?;
        }

        // If we're the new primary and we have the required 2f ViewChange messages (not
        // including our own), broadcast the NewView message
        let messages = self
            .msg_log
            .get_messages_of_type_view(PbftMessageType::ViewChange, msg_view)
            .iter()
            .cloned()
            .filter(|msg| !msg.from_self)
            .collect::<Vec<_>>();

        if state.is_primary_at_view(msg_view) && messages.len() >= 2 * state.f as usize {
            let mut new_view = PbftNewView::new();

            new_view.set_info(PbftMessageInfo::new_from(
                PbftMessageType::NewView,
                msg_view,
                state.seq_num - 1,
                state.id.clone(),
            ));

            new_view.set_view_changes(Self::signed_votes_from_messages(messages));

            let msg_bytes = new_view
                .write_to_bytes()
                .map_err(PbftError::SerializationError)?;

            self._broadcast_message(PbftMessageType::NewView, msg_bytes, state)?;
        }

        Ok(())
    }

    /// Handle a `NewView` message
    ///
    /// When a `NewView` is received, first verify that it is valid. If the NewView is invalid,
    /// start a new view change for the next view; if the NewView is valid, update the view and the
    /// node's state.
    fn handle_new_view(
        &mut self,
        msg: &ParsedMessage,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        let new_view = msg.get_new_view_message();

        match self.verify_new_view(new_view, state) {
            Err(PbftError::NotFromPrimary) => {
                // Not the new primary that's faulty, so no need to do a new view change,
                // just don't proceed any further
                return Ok(());
            }
            Err(e) => {
                if let PbftMode::ViewChanging(v) = state.mode {
                    warn!("NewView message is invalid, got error: {:?}, Starting new view change to view {}", e, v + 1);
                    self.propose_view_change(state, v + 1)?;
                    return Ok(());
                }
            }
            Ok(_) => {}
        }

        // Update view
        state.view = new_view.get_info().get_view();
        state.view_change_timeout.stop();

        // Initialize a new block if necessary
        if state.is_primary() && state.working_block.is_none() {
            self.service
                .initialize_block(None)
                .unwrap_or_else(|err| error!("Couldn't initialize block: {}", err));
        }

        state.reset_to_start();

        Ok(())
    }

    /// Handle a `BlockNew` update from the Validator
    ///
    /// The validator has received a new block; verify the block's consensus seal and add the
    /// BlockNew to the message log. If this is the block we are waiting for: set it as the working
    /// block, update the idle & commit timers, and broadcast a PrePrepare if this node is the
    /// primary. If this is the block after the one this node is working on, use it to catch up.
    pub fn on_block_new(&mut self, block: Block, state: &mut PbftState) -> Result<(), PbftError> {
        info!(
            "{}: Got BlockNew: {} / {}",
            state,
            block.block_num,
            hex::encode(&block.block_id[..3]),
        );

        if block.block_num < state.seq_num {
            info!(
                "Ignoring block ({}) that's older than current sequence number ({}).",
                block.block_num, state.seq_num
            );
            return Ok(());
        }

        match self.verify_consensus_seal(&block, state) {
            Ok(_) => {}
            Err(err) => {
                warn!(
                    "Failing block due to failed consensus seal verification and \
                     proposing view change! Error was {}",
                    err
                );
                self.service.fail_block(block.block_id).map_err(|err| {
                    PbftError::InternalError(format!("Couldn't fail block: {}", err))
                })?;
                self.propose_view_change(state, state.view + 1)?;
                return Err(err);
            }
        }

        // Create PBFT message for BlockNew and add it to the log
        let mut msg = PbftMessage::new();
        msg.set_info(PbftMessageInfo::new_from(
            PbftMessageType::BlockNew,
            state.view,
            block.block_num,
            state.id.clone(),
        ));

        let pbft_block = PbftBlock::from(block.clone());
        msg.set_block(pbft_block.clone());

        self.msg_log
            .add_message(ParsedMessage::from_pbft_message(msg.clone()), state)?;

        // We can use this block's seal to commit the next block (i.e. catch-up) if it's the block
        // after the one we're waiting for and we haven't already told the validator to commit the
        // block we're waiting for
        if block.block_num == state.seq_num + 1 && state.phase != PbftPhase::Finished {
            self.catchup(state, &block)?;
        } else if block.block_num == state.seq_num {
            // This is the block we're waiting for, so we update state
            state.working_block = Some(msg.get_block().clone());

            // Send PrePrepare messages if we're the primary
            if state.is_primary() {
                let s = state.seq_num;
                self._broadcast_pbft_message(s, PbftMessageType::PrePrepare, pbft_block, state)?;
            }
        }

        Ok(())
    }

    /// Use the given block's consensus seal to verify and commit the block this node is working on
    fn catchup(&mut self, state: &mut PbftState, block: &Block) -> Result<(), PbftError> {
        info!(
            "{}: Trying catchup to #{} from BlockNew message #{}",
            state, state.seq_num, block.block_num,
        );

        match state.working_block {
            Some(ref working_block) => {
                let block_num_matches = block.block_num == working_block.get_block_num() + 1;
                let block_id_matches = block.previous_id == working_block.get_block_id();

                if !block_num_matches || !block_id_matches {
                    error!(
                        "Block didn't match for catchup: {:?} {:?}",
                        block, working_block
                    );
                    return Err(PbftError::MismatchedBlocks(vec![
                        PbftBlock::from(block.clone()),
                        working_block.clone(),
                    ]));
                }
            }
            None => {
                error!(
                    "Trying to catch up, but node does not have block #{} yet",
                    state.seq_num
                );
                return Err(PbftError::NoWorkingBlock);
            }
        }

        // Parse messages from the seal
        let seal: PbftSeal =
            protobuf::parse_from_bytes(&block.payload).map_err(PbftError::SerializationError)?;

        let messages =
            seal.get_previous_commit_votes()
                .iter()
                .try_fold(Vec::new(), |mut msgs, v| {
                    msgs.push(ParsedMessage::from_pbft_message(
                        protobuf::parse_from_bytes(&v.get_message_bytes())
                            .map_err(PbftError::SerializationError)?,
                    ));
                    Ok(msgs)
                })?;

        // Update our view if necessary
        let view = messages[0].info().get_view();
        if view > state.view {
            info!("Updating view from {} to {}.", state.view, view);
            state.view = view;
        }

        // Add messages to the log
        for message in &messages {
            self.msg_log.add_message(message.clone(), state)?;
        }

        // Commit the new block using one of the parsed messages and skip straight to Finished
        self.service
            .commit_block(messages[0].get_block().block_id.clone())
            .map_err(|e| PbftError::InternalError(format!("Failed to commit block: {:?}", e)))?;
        state.phase = PbftPhase::Finished;

        // Call on_block_commit right away so we're ready to catch up again if necessary
        self.on_block_commit(BlockId::from(messages[0].get_block().get_block_id()), state);

        Ok(())
    }

    /// Handle a `BlockValid` update from the Validator
    ///
    /// This message arrives after `check_blocks` is called, signifying that the validator has
    /// successfully checked a block with this `BlockId`. Once a `BlockValid` is received for the
    /// working block, transition to the Preparing phase.
    #[allow(clippy::ptr_arg)]
    pub fn on_block_valid(
        &mut self,
        block_id: &BlockId,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        debug!("{}: <<<<<< BlockValid: {:?}", state, block_id);
        let block = match state.working_block {
            Some(ref block) => {
                if &BlockId::from(block.get_block_id()) == block_id {
                    Ok(block.clone())
                } else {
                    warn!("Got BlockValid that doesn't match the working block");
                    Err(PbftError::NotReadyForMessage)
                }
            }
            None => {
                warn!("Got BlockValid with no working block");
                Err(PbftError::NoWorkingBlock)
            }
        }?;

        state.switch_phase(PbftPhase::Preparing);
        self._broadcast_pbft_message(
            state.seq_num,
            PbftMessageType::Prepare,
            block.clone(),
            state,
        )?;
        Ok(())
    }

    /// Handle a `BlockCommit` update from the Validator
    ///
    /// A block was sucessfully committed; update state to be ready for the next block, make any
    /// necessary view and membership changes, garbage collect the logs, update the commit & idle
    /// timers, and start a new block if this node is the primary.
    #[allow(clippy::needless_pass_by_value)]
    pub fn on_block_commit(&mut self, block_id: BlockId, state: &mut PbftState) {
        debug!("{}: <<<<<< BlockCommit: {:?}", state, block_id);

        let is_working_block = match state.working_block {
            Some(ref block) => BlockId::from(block.get_block_id()) == block_id,
            None => false,
        };

        if state.phase != PbftPhase::Finished || !is_working_block {
            info!(
                "{}: Got BlockCommit for a block that isn't the working block",
                state
            );
            return;
        }

        // Update state to be ready for next block
        state.switch_phase(PbftPhase::PrePreparing);
        state.seq_num += 1;

        // If we already have a BlockNew for the next block, we can make it the working block;
        // otherwise just set the working block to None
        state.working_block = self
            .msg_log
            .get_messages_of_type_seq(PbftMessageType::BlockNew, state.seq_num)
            .first()
            .map(|msg| msg.get_block().clone());

        // Increment the view if we need to force a view change for fairness or if membership
        // has changed
        if state.at_forced_view_change() || self.update_membership(block_id.clone(), state) {
            state.view += 1;
        }

        // Tell the log to garbage collect if it needs to
        self.msg_log.garbage_collect(state.seq_num);

        // Restart the faulty primary timeout for the next block
        state.faulty_primary_timeout.start();

        if state.is_primary() && state.working_block.is_none() {
            info!(
                "{}: Initializing block with previous ID {:?}",
                state, block_id
            );
            self.service
                .initialize_block(Some(block_id.clone()))
                .unwrap_or_else(|err| error!("Couldn't initialize block: {}", err));
        }
    }

    /// Check the on-chain list of peers; if it has changed, update peers list and return true.
    fn update_membership(&mut self, block_id: BlockId, state: &mut PbftState) -> bool {
        // Get list of peers from settings
        let settings = self
            .service
            .get_settings(
                block_id,
                vec![String::from("sawtooth.consensus.pbft.peers")],
            )
            .expect("Failed to get settings");
        let peers = get_peers_from_settings(&settings);
        let new_peers_set: HashSet<PeerId> = peers.iter().cloned().collect();

        // Check if membership has changed
        let old_peers_set: HashSet<PeerId> = state.peer_ids.iter().cloned().collect();

        if new_peers_set != old_peers_set {
            state.peer_ids = peers;
            let f = ((state.peer_ids.len() - 1) / 3) as u64;
            if f == 0 {
                panic!("This network no longer contains enough nodes to be fault tolerant");
            }
            state.f = f;
            return true;
        }

        false
    }

    // ---------- Methods for building & verifying proofs and signed messages from other nodes ----------

    /// Generate a `protobuf::RepeatedField` of signed votes from a list of parsed messages
    #[allow(clippy::needless_pass_by_value)]
    fn signed_votes_from_messages(msgs: Vec<&ParsedMessage>) -> RepeatedField<PbftSignedVote> {
        RepeatedField::from(
            msgs.iter()
                .map(|m| {
                    let mut vote = PbftSignedVote::new();

                    vote.set_header_bytes(m.header_bytes.clone());
                    vote.set_header_signature(m.header_signature.clone());
                    vote.set_message_bytes(m.message_bytes.clone());

                    vote
                })
                .collect::<Vec<_>>(),
        )
    }

    /// Build a consensus seal to be put in the block that matches the `summary` and proves the
    /// last block committed by this node
    fn build_seal(&mut self, state: &PbftState, summary: Vec<u8>) -> Result<Vec<u8>, PbftError> {
        info!("{}: Building seal for block {}", state, state.seq_num - 1);

        let min_votes = 2 * state.f;
        let messages = self
            .msg_log
            .get_enough_messages(PbftMessageType::Commit, state.seq_num - 1, min_votes)
            .ok_or_else(|| {
                debug!("{}: {}", state, self.msg_log);
                PbftError::InternalError(format!(
                    "Couldn't find {} commit messages in the message log for building a seal!",
                    min_votes
                ))
            })?;

        let mut seal = PbftSeal::new();

        seal.set_summary(summary);
        seal.set_previous_id(BlockId::from(messages[0].get_block().get_block_id()));
        seal.set_previous_commit_votes(Self::signed_votes_from_messages(messages));

        seal.write_to_bytes().map_err(PbftError::SerializationError)
    }

    /// Verify that a vote matches the expected type, is properly signed, and passes the specified
    /// criteria; if it passes verification, return the signer ID to be used for further
    /// verification
    fn verify_vote<F>(
        vote: &PbftSignedVote,
        expected_type: PbftMessageType,
        validation_criteria: F,
    ) -> Result<PeerId, PbftError>
    where
        F: Fn(&PbftMessage) -> Result<(), PbftError>,
    {
        // Parse the message
        let pbft_message: PbftMessage = protobuf::parse_from_bytes(&vote.get_message_bytes())
            .map_err(PbftError::SerializationError)?;
        let header: ConsensusPeerMessageHeader =
            protobuf::parse_from_bytes(&vote.get_header_bytes())
                .map_err(PbftError::SerializationError)?;

        // Verify the message type
        let msg_type = PbftMessageType::from(pbft_message.get_info().get_msg_type());
        if msg_type != expected_type {
            return Err(PbftError::InternalError(format!(
                "Received a {:?} vote, but expected a {:?}",
                msg_type, expected_type
            )));
        }

        // Verify the signature
        let key = Secp256k1PublicKey::from_hex(&hex::encode(&header.signer_id)).unwrap();
        let context = create_context("secp256k1")
            .map_err(|err| PbftError::InternalError(format!("Couldn't create context: {}", err)))?;

        match context.verify(
            &hex::encode(vote.get_header_signature()),
            vote.get_header_bytes(),
            &key,
        ) {
            Ok(true) => {}
            Ok(false) => {
                return Err(PbftError::InternalError(
                    "Header failed verification!".into(),
                ))
            }
            Err(err) => {
                return Err(PbftError::InternalError(format!(
                    "Error while verifying header: {:?}",
                    err
                )))
            }
        }

        verify_sha512(vote.get_message_bytes(), header.get_content_sha512())?;

        // Validate against the specified criteria
        validation_criteria(&pbft_message)?;

        Ok(PeerId::from(pbft_message.get_info().get_signer_id()))
    }

    /// Verify that a NewView messsage is valid
    fn verify_new_view(
        &mut self,
        new_view: &PbftNewView,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // Make sure this is from the new primary
        if PeerId::from(new_view.get_info().get_signer_id())
            != state.get_primary_id_at_view(new_view.get_info().get_view())
        {
            error!(
                "Got NewView message ({:?}) from node that is not primary for new view",
                new_view,
            );
            return Err(PbftError::NotFromPrimary);
        }

        // Verify each individual vote, and extract the signer ID from each ViewChange that
        // it contains so we can verify the IDs themselves
        let voter_ids =
            new_view
                .get_view_changes()
                .iter()
                .try_fold(HashSet::new(), |mut ids, vote| {
                    Self::verify_vote(vote, PbftMessageType::ViewChange, |msg| {
                        if msg.get_info().get_view() != new_view.get_info().get_view() {
                            return Err(PbftError::InternalError(format!(
                                "ViewChange ({:?}) doesn't match NewView ({:?})",
                                msg, &new_view,
                            )));
                        }
                        Ok(())
                    })
                    .and_then(|id| Ok(ids.insert(id)))?;
                    Ok(ids)
                })?;

        // All of the votes must come from known peers, and the new primary can't
        // explicitly vote itself, since broacasting the NewView is an implicit vote. Check
        // that the votes we've received are a subset of "peers - primary".
        let peer_ids: HashSet<_> = state
            .peer_ids
            .iter()
            .cloned()
            .filter(|pid| pid != &PeerId::from(new_view.get_info().get_signer_id()))
            .collect();

        if !voter_ids.is_subset(&peer_ids) {
            return Err(PbftError::InternalError(format!(
                "Got unexpected vote IDs when verifying NewView: {:?}",
                voter_ids.difference(&peer_ids).collect::<Vec<_>>()
            )));
        }

        // Check that we've received 2f votes, since the primary vote is implicit
        if voter_ids.len() < 2 * state.f as usize {
            return Err(PbftError::InternalError(format!(
                "Need {} votes, only found {}!",
                2 * state.f,
                voter_ids.len()
            )));
        }

        Ok(())
    }

    /// Verify the consensus seal from the current block that proves the previous block
    fn verify_consensus_seal(
        &mut self,
        block: &Block,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // We don't publish a consensus seal until block 1, so we don't verify it
        // until block 2
        if block.block_num < 2 {
            return Ok(());
        }

        if block.payload.is_empty() {
            return Err(PbftError::InternalError(
                "Got empty payload for non-genesis block!".into(),
            ));
        }

        let seal: PbftSeal =
            protobuf::parse_from_bytes(&block.payload).map_err(PbftError::SerializationError)?;

        if seal.previous_id != &block.previous_id[..] {
            return Err(PbftError::InternalError(format!(
                "Consensus seal failed verification. Seal's previous ID `{}` doesn't match block's previous ID `{}`",
                hex::encode(&seal.previous_id[..3]), hex::encode(&block.previous_id[..3])
            )));
        }

        if seal.summary != &block.summary[..] {
            return Err(PbftError::InternalError(format!(
                "Consensus seal failed verification. Seal's summary {:?} doesn't match block's summary {:?}",
                seal.summary, block.summary
            )));
        }

        // Verify each individual vote, and extract the signer ID from each PbftMessage that
        // it contains, so that we can do some sanity checks on those IDs.
        let voter_ids =
            seal.get_previous_commit_votes()
                .iter()
                .try_fold(HashSet::new(), |mut ids, vote| {
                    Self::verify_vote(vote, PbftMessageType::Commit, |msg| {
                        if msg.get_block().block_id != seal.previous_id {
                            return Err(PbftError::InternalError(format!(
                            "PbftMessage block ID ({:?}) doesn't match seal's previous id ({:?})!",
                            msg.get_block().block_id,
                            seal.previous_id
                        )));
                        }
                        Ok(())
                    })
                    .and_then(|id| Ok(ids.insert(id)))?;
                    Ok(ids)
                })?;

        // All of the votes must come from known peers, and the primary can't explicitly
        // vote itself, since publishing a block is an implicit vote. Check that the votes
        // we've received are a subset of "peers - primary". We need to use the list of
        // peers from the block we're verifying the seal for, since it may have changed.
        let settings = self
            .service
            .get_settings(
                block.previous_id.clone(),
                vec![String::from("sawtooth.consensus.pbft.peers")],
            )
            .expect("Failed to get settings");
        let peers = get_peers_from_settings(&settings);

        let peer_ids: HashSet<_> = peers
            .iter()
            .cloned()
            .filter(|pid| pid != &block.signer_id)
            .collect();

        if !voter_ids.is_subset(&peer_ids) {
            return Err(PbftError::InternalError(format!(
                "Got unexpected vote IDs: {:?}",
                voter_ids.difference(&peer_ids).collect::<Vec<_>>()
            )));
        }

        // Check that we've received 2f votes, since the primary vote is implicit
        if voter_ids.len() < 2 * state.f as usize {
            return Err(PbftError::InternalError(format!(
                "Need {} votes, only found {}!",
                2 * state.f,
                voter_ids.len()
            )));
        }

        Ok(())
    }

    // ---------- Methods called in the main engine loop to periodically check and update state ----------

    /// At a regular interval, try to finalize a block when the primary is ready
    pub fn try_publish(&mut self, state: &mut PbftState) -> Result<(), PbftError> {
        // Only the primary takes care of this, and we try publishing a block
        // on every engine loop, even if it's not yet ready. This isn't an error,
        // so just return Ok(()).
        if !state.is_primary() || state.phase != PbftPhase::PrePreparing {
            return Ok(());
        }

        info!("{}: Summarizing block", state);

        let summary = match self.service.summarize_block() {
            Ok(bytes) => bytes,
            Err(e) => {
                debug!(
                    "{}: Couldn't summarize, so not finalizing: {}",
                    state,
                    e.description().to_string()
                );
                return Ok(());
            }
        };

        // We don't publish a consensus seal at block 1, since we never receive any
        // votes on the genesis block. Leave payload blank for the first block.
        let data = if state.seq_num <= 1 {
            vec![]
        } else {
            self.build_seal(state, summary)?
        };

        match self.service.finalize_block(data) {
            Ok(block_id) => {
                info!("{}: Publishing block {:?}", state, block_id);
                Ok(())
            }
            Err(EngineError::BlockNotReady) => {
                debug!("{}: Block not ready", state);
                Ok(())
            }
            Err(err) => {
                error!("Couldn't finalize block: {}", err);
                Err(PbftError::InternalError("Couldn't finalize block!".into()))
            }
        }
    }

    /// Check to see if the faulty primary timeout has expired
    pub fn check_faulty_primary_timeout_expired(&mut self, state: &mut PbftState) -> bool {
        state.faulty_primary_timeout.check_expired()
    }

    /// Start the faulty primary timeout
    pub fn start_faulty_primary_timeout(&self, state: &mut PbftState) {
        state.faulty_primary_timeout.start();
    }

    /// Check to see if the view change timeout has expired
    pub fn check_view_change_timeout_expired(&mut self, state: &mut PbftState) -> bool {
        state.view_change_timeout.check_expired()
    }

    /// Retry messages from the backlog queue
    pub fn retry_backlog(&mut self, state: &mut PbftState) -> Result<(), PbftError> {
        let mut peer_res = Ok(());
        if let Some(msg) = self.msg_log.pop_backlog() {
            debug!("{}: Popping message from backlog", state);
            peer_res = self.on_peer_message(msg, state);
        }
        peer_res
    }

    // ---------- Methods for communication between nodes ----------

    /// Verify that the specified message type should be sent, construct the message bytes, and
    /// broadcast the message to all of this node's peers and itself
    fn _broadcast_pbft_message(
        &mut self,
        seq_num: u64,
        msg_type: PbftMessageType,
        block: PbftBlock,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // Make sure that we should be sending messages of this type
        let expected_type = state.check_msg_type();
        if msg_type.is_multicast() && msg_type != expected_type {
            return Ok(());
        }

        let mut msg = PbftMessage::new();
        msg.set_info(PbftMessageInfo::new_from(
            msg_type,
            state.view,
            seq_num,
            state.id.clone(),
        ));
        msg.set_block(block);

        self._broadcast_message(msg_type, msg.write_to_bytes().unwrap_or_default(), state)
    }

    /// Broadcast the specified message to all of the node's peers, including itself
    #[cfg(not(test))]
    fn _broadcast_message(
        &mut self,
        msg_type: PbftMessageType,
        msg: Vec<u8>,
        state: &mut PbftState,
    ) -> Result<(), PbftError> {
        // Broadcast to peers
        debug!("{}: Broadcasting {:?}", state, msg_type);
        self.service
            .broadcast(String::from(msg_type).as_str(), msg.clone())
            .unwrap_or_else(|err| error!("Couldn't broadcast: {}", err));

        // Send to self
        let parsed_message = ParsedMessage::from_bytes(msg)?;

        self.on_peer_message(parsed_message, state)
    }

    /// Disabled self-sending (used for testing)
    #[cfg(test)]
    fn _broadcast_message(
        &mut self,
        _msg_type: PbftMessageType,
        _msg: Vec<u8>,
        _state: &mut PbftState,
    ) -> Result<(), PbftError> {
        return Ok(());
    }

    // ---------- Miscellaneous methods ----------

    /// Initiate a view change when this node suspects that the primary is faulty
    pub fn propose_view_change(
        &mut self,
        state: &mut PbftState,
        view: u64,
    ) -> Result<(), PbftError> {
        // Do not send messages again if we are already in the midst of this or a later view change
        if match state.mode {
            PbftMode::ViewChanging(v) => view <= v,
            _ => false,
        } {
            return Ok(());
        }

        warn!("{}: Starting view change", state);
        state.mode = PbftMode::ViewChanging(view);

        // Update the view change timeout and start it
        state.view_change_timeout = Timeout::new(
            state
                .view_change_duration
                .checked_mul((view - state.view) as u32)
                .expect("View change timeout has overflowed."),
        );
        state.view_change_timeout.start();

        let mut vc_msg = PbftMessage::new();
        vc_msg.set_info(PbftMessageInfo::new_from(
            PbftMessageType::ViewChange,
            view,
            state.seq_num - 1,
            state.id.clone(),
        ));

        let msg_bytes = vc_msg
            .write_to_bytes()
            .map_err(PbftError::SerializationError)?;

        self._broadcast_message(PbftMessageType::ViewChange, msg_bytes, state)
    }
}

/// NOTE: Testing the PbftNode is a bit strange. Due to missing functionality in the Service,
/// a node calling `broadcast()` doesn't include sending a message to itself. In order to get around
/// this, `on_peer_message()` is called, which sometimes causes unintended side effects when
/// testing. Self-sending has been disabled (see `broadcast()` method) for testing purposes.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::mock_config;
    use crate::hash::{hash_sha256, hash_sha512};
    use crate::protos::pbft_message::PbftMessageInfo;
    use sawtooth_sdk::consensus::engine::{Error, PeerId};
    use sawtooth_sdk::messages::consensus::ConsensusPeerMessageHeader;
    use serde_json;
    use std::collections::HashMap;
    use std::default::Default;
    use std::fs::{remove_file, File};
    use std::io::prelude::*;

    const BLOCK_FILE: &str = "target/blocks.txt";

    /// Mock service to roughly keep track of the blockchain
    pub struct MockService {
        pub chain: Vec<BlockId>,
    }

    impl MockService {
        /// Serialize the chain into JSON, and write to a file
        fn write_chain(&self) {
            let mut block_file = File::create(BLOCK_FILE).unwrap();
            let block_bytes: Vec<Vec<u8>> = self
                .chain
                .iter()
                .map(|block: &BlockId| -> Vec<u8> { Vec::<u8>::from(block.clone()) })
                .collect();

            let ser_blocks = serde_json::to_string(&block_bytes).unwrap();
            block_file.write_all(&ser_blocks.into_bytes()).unwrap();
        }
    }

    impl Service for MockService {
        fn send_to(
            &mut self,
            _peer: &PeerId,
            _message_type: &str,
            _payload: Vec<u8>,
        ) -> Result<(), Error> {
            Ok(())
        }
        fn broadcast(&mut self, _message_type: &str, _payload: Vec<u8>) -> Result<(), Error> {
            Ok(())
        }
        fn initialize_block(&mut self, _previous_id: Option<BlockId>) -> Result<(), Error> {
            Ok(())
        }
        fn summarize_block(&mut self) -> Result<Vec<u8>, Error> {
            Ok(Default::default())
        }
        fn finalize_block(&mut self, _data: Vec<u8>) -> Result<BlockId, Error> {
            Ok(Default::default())
        }
        fn cancel_block(&mut self) -> Result<(), Error> {
            Ok(())
        }
        fn check_blocks(&mut self, _priority: Vec<BlockId>) -> Result<(), Error> {
            Ok(())
        }
        fn commit_block(&mut self, block_id: BlockId) -> Result<(), Error> {
            self.chain.push(block_id);
            self.write_chain();
            Ok(())
        }
        fn ignore_block(&mut self, _block_id: BlockId) -> Result<(), Error> {
            Ok(())
        }
        fn fail_block(&mut self, _block_id: BlockId) -> Result<(), Error> {
            Ok(())
        }
        fn get_blocks(
            &mut self,
            block_ids: Vec<BlockId>,
        ) -> Result<HashMap<BlockId, Block>, Error> {
            let mut res = HashMap::new();
            for id in &block_ids {
                let index = self
                    .chain
                    .iter()
                    .position(|val| val == id)
                    .unwrap_or(self.chain.len());
                res.insert(id.clone(), mock_block(index as u64));
            }
            Ok(res)
        }
        fn get_chain_head(&mut self) -> Result<Block, Error> {
            let prev_num = self.chain.len().checked_sub(2).unwrap_or(0);
            Ok(Block {
                block_id: self.chain.last().unwrap().clone(),
                previous_id: self.chain.get(prev_num).unwrap().clone(),
                signer_id: PeerId::from(vec![]),
                block_num: self.chain.len().checked_sub(1).unwrap_or(0) as u64,
                payload: vec![],
                summary: vec![],
            })
        }
        fn get_settings(
            &mut self,
            _block_id: BlockId,
            _settings: Vec<String>,
        ) -> Result<HashMap<String, String>, Error> {
            let mut settings: HashMap<String, String> = Default::default();
            settings.insert(
                "sawtooth.consensus.pbft.peers".to_string(),
                "[\"00\", \"01\", \"02\", \"03\"]".to_string(),
            );
            Ok(settings)
        }
        fn get_state(
            &mut self,
            _block_id: BlockId,
            _addresses: Vec<String>,
        ) -> Result<HashMap<String, Vec<u8>>, Error> {
            Ok(Default::default())
        }
    }

    /// Create a node, based on a given ID
    fn mock_node(node_id: PeerId) -> PbftNode {
        let service: Box<MockService> = Box::new(MockService {
            // Create genesis block (but with actual ID)
            chain: vec![mock_block_id(0)],
        });
        let cfg = mock_config(4);
        PbftNode::new(&cfg, service, node_id == vec![0])
    }

    /// Create a deterministic BlockId hash based on a block number
    fn mock_block_id(num: u64) -> BlockId {
        BlockId::from(hash_sha256(
            format!("I'm a block with block num {}", num).as_bytes(),
        ))
    }

    /// Create a mock Block, including only the BlockId, the BlockId of the previous block, and the
    /// block number
    fn mock_block(num: u64) -> Block {
        Block {
            block_id: mock_block_id(num),
            previous_id: mock_block_id(num - 1),
            signer_id: PeerId::from(vec![]),
            block_num: num,
            payload: vec![],
            summary: vec![],
        }
    }

    /// Creates a block with a valid consensus seal for the previous block
    fn mock_block_with_seal(num: u64, node: &mut PbftNode, state: &mut PbftState) -> Block {
        let head = mock_block(num - 1);
        let mut block = mock_block(num);
        block.summary = vec![1, 2, 3];
        let context = create_context("secp256k1").unwrap();

        for i in 0..3 {
            let mut info = PbftMessageInfo::new();
            info.set_msg_type("Commit".into());
            info.set_view(0);
            info.set_seq_num(num - 1);
            info.set_signer_id(vec![i]);

            let mut block = PbftBlock::new();
            block.set_block_id(head.block_id.clone());

            let mut msg = PbftMessage::new();
            msg.set_info(info);
            msg.set_block(block);

            let mut message = ParsedMessage::from_pbft_message(msg);

            let key = context.new_random_private_key().unwrap();
            let pub_key = context.get_public_key(&*key).unwrap();
            let mut header = ConsensusPeerMessageHeader::new();

            header.set_signer_id(pub_key.as_slice().to_vec());
            header.set_content_sha512(hash_sha512(&message.message_bytes));

            let header_bytes = header.write_to_bytes().unwrap();
            let header_signature =
                hex::decode(context.sign(&header_bytes, &*key).unwrap()).unwrap();

            message.from_self = false;
            message.header_bytes = header_bytes;
            message.header_signature = header_signature;

            node.msg_log.add_message(message, state).unwrap();
        }

        block.payload = node.build_seal(state, vec![1, 2, 3]).unwrap();

        block
    }

    /// Create a signed ViewChange message
    fn mock_view_change(view: u64, seq_num: u64, peer: PeerId, from_self: bool) -> ParsedMessage {
        let context = create_context("secp256k1").unwrap();
        let key = context.new_random_private_key().unwrap();
        let pub_key = context.get_public_key(&*key).unwrap();

        let mut vc_msg = PbftMessage::new();
        let info = PbftMessageInfo::new_from(PbftMessageType::ViewChange, view, seq_num, peer);
        vc_msg.set_info(info);

        let mut message = ParsedMessage::from_pbft_message(vc_msg);
        let mut header = ConsensusPeerMessageHeader::new();
        header.set_signer_id(pub_key.as_slice().to_vec());
        header.set_content_sha512(hash_sha512(&message.message_bytes));
        let header_bytes = header.write_to_bytes().unwrap();
        let header_signature = hex::decode(context.sign(&header_bytes, &*key).unwrap()).unwrap();
        message.from_self = from_self;
        message.header_bytes = header_bytes;
        message.header_signature = header_signature;

        message
    }

    /// Create a mock serialized PbftMessage
    fn mock_msg(
        msg_type: PbftMessageType,
        view: u64,
        seq_num: u64,
        block: Block,
        from: PeerId,
    ) -> ParsedMessage {
        let info = PbftMessageInfo::new_from(msg_type, view, seq_num, from);

        let mut pbft_msg = PbftMessage::new();
        pbft_msg.set_info(info);
        pbft_msg.set_block(PbftBlock::from(block));

        ParsedMessage::from_pbft_message(pbft_msg)
    }

    fn handle_pbft_err(e: PbftError) {
        match e {
            PbftError::Timeout => (),
            PbftError::WrongNumMessages(_, _, _) | PbftError::NotReadyForMessage => {
                println!("{}", e)
            }
            _ => panic!("{}", e),
        }
    }

    /// Make sure that receiving a `BlockNew` update works as expected for block #1
    #[test]
    fn block_new_initial() {
        // NOTE: Special case for primary node
        let mut node0 = mock_node(vec![0]);
        let cfg = mock_config(4);
        let mut state0 = PbftState::new(vec![0], 0, &cfg);
        node0.on_block_new(mock_block(1), &mut state0).unwrap();
        assert_eq!(state0.phase, PbftPhase::PrePreparing);
        assert_eq!(state0.seq_num, 1);
        assert_eq!(state0.working_block, Some(PbftBlock::from(mock_block(1))));

        // Try the next block
        let mut node1 = mock_node(vec![1]);
        let mut state1 = PbftState::new(vec![], 0, &cfg);
        node1
            .on_block_new(mock_block(1), &mut state1)
            .unwrap_or_else(handle_pbft_err);
        assert_eq!(state1.phase, PbftPhase::PrePreparing);
        assert_eq!(state1.working_block, Some(PbftBlock::from(mock_block(1))));
    }

    #[test]
    fn block_new_first_10_blocks() {
        let mut node = mock_node(vec![0]);
        let cfg = mock_config(4);
        let mut state = PbftState::new(vec![0], 0, &cfg);

        let block_0_id = mock_block_id(0);

        // Assert starting state
        let head = node.service.get_chain_head().unwrap();
        assert_eq!(head.block_num, 0);
        assert_eq!(head.block_id, block_0_id);
        assert_eq!(head.previous_id, block_0_id);

        assert_eq!(state.id, vec![0]);
        assert_eq!(state.view, 0);
        assert_eq!(state.phase, PbftPhase::PrePreparing);
        assert_eq!(state.mode, PbftMode::Normal);
        assert_eq!(state.peer_ids, (0..4).map(|i| vec![i]).collect::<Vec<_>>());
        assert_eq!(state.f, 1);
        assert_eq!(state.forced_view_change_period, 30);
        assert_eq!(state.working_block, None);
        assert!(state.is_primary());

        // Handle the first block and assert resulting state
        node.on_block_new(mock_block(1), &mut state).unwrap();

        let head = node.service.get_chain_head().unwrap();
        assert_eq!(head.block_num, 0);
        assert_eq!(head.block_id, block_0_id);
        assert_eq!(head.previous_id, block_0_id);

        assert_eq!(state.id, vec![0]);
        assert_eq!(state.seq_num, 1);
        assert_eq!(state.view, 0);
        assert_eq!(state.phase, PbftPhase::PrePreparing);
        assert_eq!(state.mode, PbftMode::Normal);
        assert_eq!(state.peer_ids, (0..4).map(|i| vec![i]).collect::<Vec<_>>());
        assert_eq!(state.f, 1);
        assert_eq!(state.forced_view_change_period, 30);
        assert_eq!(state.working_block, Some(PbftBlock::from(mock_block(1))));
        assert!(state.is_primary());

        state.seq_num += 1;

        // Handle the rest of the blocks
        for i in 2..10 {
            assert_eq!(state.seq_num, i);
            let block = mock_block_with_seal(i, &mut node, &mut state);
            node.on_block_new(block.clone(), &mut state).unwrap();

            assert_eq!(state.id, vec![0]);
            assert_eq!(state.view, 0);
            assert_eq!(state.phase, PbftPhase::PrePreparing);
            assert_eq!(state.mode, PbftMode::Normal);
            assert_eq!(state.peer_ids, (0..4).map(|i| vec![i]).collect::<Vec<_>>());
            assert_eq!(state.f, 1);
            assert_eq!(state.forced_view_change_period, 30);
            assert_eq!(state.working_block, Some(PbftBlock::from(block)));
            assert!(state.is_primary());

            state.seq_num += 1;
        }
    }

    /// Make sure that `BlockNew` properly checks the consensus seal.
    #[test]
    fn block_new_consensus() {
        let cfg = mock_config(4);
        let mut node = mock_node(vec![1]);
        let mut state = PbftState::new(vec![], 0, &cfg);
        state.seq_num = 7;
        let head = mock_block(6);
        let mut block = mock_block(7);
        block.summary = vec![1, 2, 3];
        let context = create_context("secp256k1").unwrap();

        for i in 0..3 {
            let mut info = PbftMessageInfo::new();
            info.set_msg_type("Commit".into());
            info.set_view(0);
            info.set_seq_num(6);
            info.set_signer_id(vec![i]);

            let mut block = PbftBlock::new();
            block.set_block_id(head.block_id.clone());

            let mut msg = PbftMessage::new();
            msg.set_info(info);
            msg.set_block(block);

            let mut message = ParsedMessage::from_pbft_message(msg);

            let key = context.new_random_private_key().unwrap();
            let pub_key = context.get_public_key(&*key).unwrap();
            let mut header = ConsensusPeerMessageHeader::new();

            header.set_signer_id(pub_key.as_slice().to_vec());
            header.set_content_sha512(hash_sha512(&message.message_bytes));

            let header_bytes = header.write_to_bytes().unwrap();
            let header_signature =
                hex::decode(context.sign(&header_bytes, &*key).unwrap()).unwrap();

            message.from_self = false;
            message.header_bytes = header_bytes;
            message.header_signature = header_signature;

            node.msg_log.add_message(message, &state).unwrap();
        }

        let seal = node.build_seal(&state, vec![1, 2, 3]).unwrap();
        block.payload = seal;

        node.on_block_new(block, &mut state).unwrap();
    }

    /// Make sure that a valid `PrePrepare` is accepted
    #[test]
    fn test_pre_prepare() {
        let cfg = mock_config(4);
        let mut node0 = mock_node(vec![0]);
        let mut state0 = PbftState::new(vec![0], 0, &cfg);

        // Add BlockNew to log
        let block_new = mock_msg(PbftMessageType::BlockNew, 0, 1, mock_block(1), vec![0]);
        node0
            .msg_log
            .add_message(block_new, &state0)
            .unwrap_or_else(handle_pbft_err);

        // Add PrePrepare to log
        let valid_msg = mock_msg(PbftMessageType::PrePrepare, 0, 1, mock_block(1), vec![0]);
        node0
            .handle_pre_prepare(valid_msg.clone(), &mut state0)
            .unwrap_or_else(handle_pbft_err);

        // Verify it worked
        assert!(node0.msg_log.log_has_required_msgs(
            PbftMessageType::PrePrepare,
            &valid_msg,
            true,
            1
        ));
        assert_eq!(state0.phase, PbftPhase::Checking);
    }

    /// Make sure that receiving a `BlockValid` update works as expected
    #[test]
    fn block_valid() {
        let mut node = mock_node(vec![0]);
        let cfg = mock_config(4);
        let mut state0 = PbftState::new(vec![0], 0, &cfg);
        state0.phase = PbftPhase::Checking;
        state0.working_block = Some(PbftBlock::from(mock_block(1)));
        node.on_block_valid(&mock_block_id(1), &mut state0)
            .unwrap_or_else(handle_pbft_err);
        assert_eq!(state0.phase, PbftPhase::Preparing);
    }

    /// Make sure that receiving a `BlockCommit` update works as expected
    #[test]
    fn block_commit() {
        let mut node = mock_node(vec![0]);
        let cfg = mock_config(4);
        let mut state0 = PbftState::new(vec![0], 0, &cfg);
        state0.phase = PbftPhase::Finished;
        state0.working_block = Some(PbftBlock::from(mock_block(1)));
        assert_eq!(state0.seq_num, 1);
        node.on_block_commit(mock_block_id(1), &mut state0);
        assert_eq!(state0.phase, PbftPhase::PrePreparing);
        assert_eq!(state0.working_block, None);
        assert_eq!(state0.seq_num, 2);
    }

    /// Test the multicast protocol (`PrePrepare` => `Prepare` => `Commit`)
    #[test]
    fn multicast_protocol() {
        let cfg = mock_config(4);

        // Make sure BlockNew is in the log
        let mut node1 = mock_node(vec![1]);
        let mut state1 = PbftState::new(vec![], 0, &cfg);
        let block = mock_block(1);
        node1
            .on_block_new(block.clone(), &mut state1)
            .unwrap_or_else(handle_pbft_err);

        // Receive a PrePrepare
        let msg = mock_msg(PbftMessageType::PrePrepare, 0, 1, block.clone(), vec![0]);
        node1
            .on_peer_message(msg, &mut state1)
            .unwrap_or_else(handle_pbft_err);

        assert_eq!(state1.phase, PbftPhase::Checking);
        assert_eq!(state1.seq_num, 1);
        if let Some(ref blk) = state1.working_block {
            assert_eq!(BlockId::from(blk.clone().block_id), mock_block_id(1));
        } else {
            panic!("Wrong WorkingBlockOption");
        }

        // Spoof the `check_blocks()` call
        assert!(node1.on_block_valid(&mock_block_id(1), &mut state1).is_ok());

        // Receive 3 `Prepare` messages
        for peer in 0..3 {
            assert_eq!(state1.phase, PbftPhase::Preparing);
            let msg = mock_msg(PbftMessageType::Prepare, 0, 1, block.clone(), vec![peer]);
            node1
                .on_peer_message(msg, &mut state1)
                .unwrap_or_else(handle_pbft_err);
        }

        // Receive 3 `Commit` messages
        for peer in 0..3 {
            assert_eq!(state1.phase, PbftPhase::Committing);
            let msg = mock_msg(PbftMessageType::Commit, 0, 1, block.clone(), vec![peer]);
            node1
                .on_peer_message(msg, &mut state1)
                .unwrap_or_else(handle_pbft_err);
        }
        assert_eq!(state1.phase, PbftPhase::Finished);

        // Spoof the `commit_blocks()` call
        node1.on_block_commit(mock_block_id(1), &mut state1);
        assert_eq!(state1.phase, PbftPhase::PrePreparing);

        // Make sure the block was actually committed
        let mut f = File::open(BLOCK_FILE).unwrap();
        let mut buffer = String::new();
        f.read_to_string(&mut buffer).unwrap();
        let deser: Vec<Vec<u8>> = serde_json::from_str(&buffer).unwrap();
        let blocks: Vec<BlockId> = deser
            .iter()
            .filter(|&block| !block.is_empty())
            .map(|ref block| BlockId::from(block.clone().clone()))
            .collect();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1], mock_block_id(1));

        remove_file(BLOCK_FILE).unwrap();
    }

    /// Test that view changes work as expected, and that nodes take the proper roles after a view
    /// change
    #[test]
    fn view_change() {
        let mut node1 = mock_node(vec![1]);
        let cfg = mock_config(4);
        let mut state1 = PbftState::new(vec![1], 0, &cfg);

        assert!(!state1.is_primary());

        // Receive 3 `ViewChange` messages
        for peer in 0..3 {
            // It takes f + 1 `ViewChange` messages to trigger a view change, if it wasn't started
            // by `propose_view_change()`
            if peer < 2 {
                assert_eq!(state1.mode, PbftMode::Normal);
            } else {
                assert_eq!(state1.mode, PbftMode::ViewChanging(1));
            }

            node1
                .on_peer_message(mock_view_change(1, 0, vec![peer], peer == 1), &mut state1)
                .unwrap_or_else(handle_pbft_err);
        }

        // Receive `NewView` message
        let msgs: Vec<&ParsedMessage> = node1
            .msg_log
            .get_messages_of_type_view(PbftMessageType::ViewChange, 1)
            .iter()
            .cloned()
            .filter(|msg| !msg.from_self)
            .collect::<Vec<_>>();
        let mut new_view = PbftNewView::new();
        new_view.set_info(PbftMessageInfo::new_from(
            PbftMessageType::NewView,
            1,
            0,
            vec![1],
        ));
        new_view.set_view_changes(PbftNode::signed_votes_from_messages(msgs));

        node1
            .on_peer_message(ParsedMessage::from_new_view_message(new_view), &mut state1)
            .unwrap_or_else(handle_pbft_err);

        assert!(state1.is_primary());
        assert_eq!(state1.view, 1);
    }

    /// Make sure that view changes start correctly
    #[test]
    fn propose_view_change() {
        let mut node1 = mock_node(vec![1]);
        let cfg = mock_config(4);
        let mut state1 = PbftState::new(vec![], 0, &cfg);
        assert_eq!(state1.mode, PbftMode::Normal);

        let new_view = state1.view + 1;
        node1
            .propose_view_change(&mut state1, new_view)
            .unwrap_or_else(handle_pbft_err);

        assert_eq!(state1.mode, PbftMode::ViewChanging(1));
    }

    /// Test that try_publish adds in the consensus seal
    #[test]
    fn try_publish() {
        let mut node0 = mock_node(vec![0]);
        let cfg = mock_config(4);
        let mut state0 = PbftState::new(vec![0], 0, &cfg);
        let block0 = mock_block(1);
        let pbft_block0 = PbftBlock::from(block0);

        for i in 0..3 {
            let mut info = PbftMessageInfo::new();
            info.set_msg_type("Commit".into());
            info.set_view(0);
            info.set_seq_num(0);
            info.set_signer_id(vec![i]);

            let mut msg = PbftMessage::new();
            msg.set_info(info);
            node0
                .msg_log
                .add_message(ParsedMessage::from_pbft_message(msg), &state0)
                .unwrap();
        }

        state0.phase = PbftPhase::PrePreparing;
        state0.working_block = Some(pbft_block0.clone());

        node0.try_publish(&mut state0).unwrap();
    }
}
