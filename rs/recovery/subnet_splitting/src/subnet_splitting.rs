use crate::{
    admin_helper::{
        get_halt_subnet_at_cup_height_command, get_propose_to_complete_canister_migration_command,
        get_propose_to_prepare_canister_migration_command,
        get_propose_to_reroute_canister_ranges_command,
    },
    state_tool_helper::StateToolHelper,
    steps::{ComputeExpectedManifestsStep, CopyWorkDirStep, SplitStateStep, StateSplitStrategy},
};

use clap::Parser;
use ic_base_types::SubnetId;
use ic_recovery::{
    cli::{consent_given, read_optional, wait_for_confirmation},
    error::{RecoveryError, RecoveryResult},
    recovery_iterator::RecoveryIterator,
    recovery_state::{HasRecoveryState, RecoveryState},
    registry_helper::RegistryPollingStrategy,
    steps::{AdminStep, Step, UploadAndRestartStep, WaitForCUPStep},
    NeuronArgs, Recovery, RecoveryArgs, CHECKPOINTS, IC_REGISTRY_LOCAL_STORE, IC_STATE_DIR,
};
use ic_registry_routing_table::CanisterIdRange;
use ic_state_manager::manifest::{manifest_from_path, manifest_hash};
use serde::{Deserialize, Serialize};
use slog::{info, warn, Logger};
use strum::{EnumMessage, IntoEnumIterator};
use strum_macros::{EnumIter, EnumString};
use url::Url;

use std::{iter::Peekable, net::IpAddr, path::PathBuf};

const DESTINATION_WORK_DIR: &str = "destination_work_dir";

#[derive(
    Debug,
    Copy,
    Clone,
    PartialEq,
    EnumIter,
    EnumString,
    Serialize,
    Deserialize,
    EnumMessage,
    clap::ValueEnum,
)]
pub(crate) enum StepType {
    PrepareCanisterMigration,
    HaltSourceSubnetAtCupHeight,
    RerouteCanisterRanges,
    DownloadStateFromSourceSubnet,
    ComputeExpectedManifestsStep,
    CopyDir,
    SplitOutSourceState,
    SplitOutDestinationState,
    ProposeCupForSourceSubnet,
    UploadStateToSourceSubnet,
    ProposeCupForDestinationSubnet,
    UploadStateToDestinationSubnet,
    WaitForCUPOnSourceSubnet,
    WaitForCUPOnDestinationSubnet,
    UnhaltSourceSubnet,
    UnhaltDestinationSubnet,
    CompleteCanisterMigration,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Parser, Serialize, Deserialize)]
#[clap(version = "1.0")]
pub(crate) struct SubnetSplittingArgs {
    /// Id of the subnet whose state will be split.
    #[clap(long, parse(try_from_str=crate::util::subnet_id_from_str))]
    source_subnet_id: SubnetId,

    /// Id of the destination subnet.
    #[clap(long, parse(try_from_str=crate::util::subnet_id_from_str))]
    destination_subnet_id: SubnetId,

    /// Public ssh key to be deployed to the subnet for read only access.
    #[clap(long)]
    pub_key: Option<String>,

    /// If the downloaded state should be backed up locally.
    #[clap(long)]
    keep_downloaded_state: Option<bool>,

    /// IP address of the node from the source subnet to download the state from.
    #[clap(long)]
    download_node_source: Option<IpAddr>,

    /// IP address of the node to upload the new subnet state to.
    #[clap(long)]
    upload_node_source: Option<IpAddr>,

    /// IP address of the node to upload the new subnet state to.
    #[clap(long)]
    upload_node_destination: Option<IpAddr>,

    /// If present the tool will start execution for the provided step, skipping the initial ones.
    #[clap(long = "resume")]
    #[clap(value_enum)]
    next_step: Option<StepType>,

    /// The canister ID ranges to be moved to the destination subnet.
    #[clap(long, multiple_values(true), required = true)]
    canister_id_ranges_to_move: Vec<CanisterIdRange>,
}

pub(crate) struct SubnetSplitting {
    step_iterator: Peekable<StepTypeIter>,
    params: SubnetSplittingArgs,
    recovery_args: RecoveryArgs,
    neuron_args: Option<NeuronArgs>,
    recovery: Recovery,
    state_tool_helper: StateToolHelper,
    logger: Logger,
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum TargetSubnet {
    Source,
    Destination,
}

impl SubnetSplitting {
    pub(crate) fn new(
        logger: Logger,
        recovery_args: RecoveryArgs,
        neuron_args: Option<NeuronArgs>,
        subnet_args: SubnetSplittingArgs,
    ) -> Self {
        let recovery = Recovery::new(
            logger.clone(),
            recovery_args.clone(),
            neuron_args.clone(),
            recovery_args.nns_url.clone(),
            RegistryPollingStrategy::WithEveryRead,
        )
        .expect("Failed to initialize recovery");

        let state_tool_helper = StateToolHelper::new(
            recovery.binary_dir.clone(),
            recovery_args.replica_version.clone(),
            logger.clone(),
        )
        .expect("Failed to initialize state tool helper");

        Self {
            step_iterator: StepType::iter().peekable(),
            params: subnet_args,
            recovery_args,
            neuron_args,
            recovery,
            state_tool_helper,
            logger,
        }
    }

    fn split_state_step(&self, target_subnet: TargetSubnet) -> SplitStateStep {
        let state_split_strategy = match target_subnet {
            TargetSubnet::Source => {
                StateSplitStrategy::Drop(self.params.canister_id_ranges_to_move.clone())
            }
            TargetSubnet::Destination => {
                StateSplitStrategy::Retain(self.params.canister_id_ranges_to_move.clone())
            }
        };

        SplitStateStep {
            subnet_id: self.subnet_id(target_subnet),
            state_split_strategy,
            state_tool_helper: self.state_tool_helper.clone(),
            work_dir: self.work_dir(target_subnet),
            logger: self.recovery.logger.clone(),
        }
    }

    fn unhalt(&self, target_subnet: TargetSubnet) -> impl Step {
        self.recovery.halt_subnet(
            self.subnet_id(target_subnet),
            /*is_halted=*/ false,
            /*keys=*/ &[],
        )
    }

    fn propose_cup(&self, target_subnet: TargetSubnet) -> RecoveryResult<impl Step> {
        let checkpoints_dir = self
            .work_dir(target_subnet)
            .join(IC_STATE_DIR)
            .join(CHECKPOINTS);

        let (max_name, max_height) =
            Recovery::get_latest_checkpoint_name_and_height(&checkpoints_dir)?;

        let max_checkpoint_dir = checkpoints_dir.join(max_name);
        let manifest = &manifest_from_path(&max_checkpoint_dir).map_err(|e| {
            RecoveryError::CheckpointError(
                format!(
                    "Failed to read the manifest from path {}",
                    max_checkpoint_dir.display()
                ),
                e,
            )
        })?;
        let state_hash = hex::encode(manifest_hash(manifest));

        self.recovery.update_recovery_cup(
            self.subnet_id(target_subnet),
            Recovery::get_recovery_height(max_height),
            state_hash,
            /*replacement_nodes=*/ &[],
            /*registry_params=*/ None,
            /*ecdsa_subnet_id=*/ None,
        )
    }

    fn upload_and_restart_step(&self, target_subnet: TargetSubnet) -> RecoveryResult<impl Step> {
        match self.upload_node(target_subnet) {
            Some(node_ip) => Ok(UploadAndRestartStep {
                logger: self.recovery.logger.clone(),
                node_ip,
                work_dir: self.work_dir(target_subnet),
                data_src: self.work_dir(target_subnet).join(IC_STATE_DIR),
                require_confirmation: true,
                key_file: self.recovery.key_file.clone(),
                check_ic_replay_height: false,
            }),
            None => Err(RecoveryError::StepSkipped),
        }
    }

    fn wait_for_cup_step(&self, target_subnet: TargetSubnet) -> RecoveryResult<impl Step> {
        match self.upload_node(target_subnet) {
            Some(node_ip) => Ok(WaitForCUPStep {
                logger: self.recovery.logger.clone(),
                node_ip,
                work_dir: self.work_dir(target_subnet),
            }),
            None => Err(RecoveryError::StepSkipped),
        }
    }

    fn upload_node(&self, target_subnet: TargetSubnet) -> Option<IpAddr> {
        match target_subnet {
            TargetSubnet::Source => self.params.upload_node_source,
            TargetSubnet::Destination => self.params.upload_node_destination,
        }
    }

    fn subnet_id(&self, target_subnet: TargetSubnet) -> SubnetId {
        match target_subnet {
            TargetSubnet::Source => self.params.source_subnet_id,
            TargetSubnet::Destination => self.params.destination_subnet_id,
        }
    }

    fn work_dir(&self, target_subnet: TargetSubnet) -> PathBuf {
        match target_subnet {
            TargetSubnet::Source => self.recovery.work_dir.clone(),
            TargetSubnet::Destination => {
                self.recovery.work_dir.with_file_name(DESTINATION_WORK_DIR)
            }
        }
    }
}

impl RecoveryIterator<StepType, StepTypeIter> for SubnetSplitting {
    fn get_step_iterator(&mut self) -> &mut Peekable<StepTypeIter> {
        &mut self.step_iterator
    }

    fn store_next_step(&mut self, step_type: Option<StepType>) {
        self.params.next_step = step_type;
    }

    fn get_logger(&self) -> &Logger {
        &self.logger
    }

    fn interactive(&self) -> bool {
        true
    }

    fn read_step_params(&mut self, step_type: StepType) {
        match step_type {
            StepType::HaltSourceSubnetAtCupHeight => {
                let url = match self.recovery.registry_helper.latest_registry_version() {
                    Ok(registry_version) => {
                        format!(
                            "https://grafana.mainnet.dfinity.network/d/cB-qtJX4k/subnet-splitting-pre-flight?var-datasource=IC+Metrics&var-ic=mercury&var-ic_subnet={}&var-registry_version={}",
                            self.params.destination_subnet_id, registry_version
                        )
                    }
                    Err(err) => {
                        warn!(
                            self.logger,
                            "Failed to get the latest registry version: {}", err
                        );
                        format!(
                            "https://grafana.mainnet.dfinity.network/d/cB-qtJX4k/subnet-splitting-pre-flight?var-datasource=IC+Metrics&var-ic=mercury&var-ic_subnet={}",
                            self.params.destination_subnet_id
                        )
                    }
                };

                print_url_and_ask_for_confirmation(
                    &self.logger,
                    url,
                    "Please check the dashboard to see if it is safe to begin subnet splitting",
                );

                if self.params.pub_key.is_none() {
                    self.params.pub_key = read_optional(
                        &self.logger,
                        "Enter public key to add readonly SSH access to subnet: ",
                    )
                }
            }

            StepType::DownloadStateFromSourceSubnet => {
                if self.params.download_node_source.is_none() {
                    self.params.download_node_source =
                        read_optional(&self.logger, "Enter download IP on the Source Subnet:");
                }

                self.params.keep_downloaded_state = Some(consent_given(
                    &self.logger,
                    "Preserve original downloaded state locally?",
                ));
            }

            StepType::UploadStateToSourceSubnet => {
                if self.params.upload_node_source.is_none() {
                    self.params.upload_node_source = read_optional(
                        &self.logger,
                        "Enter IP of node in the Source Subnet with admin access: ",
                    );
                }
            }

            StepType::UploadStateToDestinationSubnet => {
                if self.params.upload_node_destination.is_none() {
                    self.params.upload_node_destination = read_optional(
                        &self.logger,
                        "Enter IP of node in the Destination Subnet with admin access: ",
                    );
                }
            }

            StepType::UnhaltDestinationSubnet | StepType::CompleteCanisterMigration => {
                let url = match self.recovery.registry_helper.latest_registry_version() {
                    Ok(registry_version) => {
                        format!(
                            "https://grafana.mainnet.dfinity.network/d/K08U69_4k/subnet-splitting?var-datasource=IC+Metrics&var-ic=mercury&var-ic_subnet={}&var-registry_version={}",
                            self.params.source_subnet_id, registry_version
                        )
                    }
                    Err(err) => {
                        warn!(
                            self.logger,
                            "Failed to get the latest registry version: {}", err
                        );
                        format!(
                            "https://grafana.mainnet.dfinity.network/d/K08U69_4k/subnet-splitting?var-datasource=IC+Metrics&var-ic=mercury&var-ic_subnet={}",
                            self.params.source_subnet_id,
                        )
                    }
                };

                print_url_and_ask_for_confirmation(
                    &self.logger,
                    url,
                    "Please check the dashboard to see if it is safe to unhalt the \
                    destination subnet and/or remove the canister migrations entry",
                );
            }

            _ => (),
        }
    }

    fn get_step_impl(&self, step_type: StepType) -> RecoveryResult<Box<dyn Step>> {
        let step: Box<dyn Step> = match step_type {
            StepType::PrepareCanisterMigration => AdminStep {
                logger: self.recovery.logger.clone(),
                ic_admin_cmd: get_propose_to_prepare_canister_migration_command(
                    &self.recovery.admin_helper,
                    &self.params.canister_id_ranges_to_move,
                    self.params.source_subnet_id,
                    self.params.destination_subnet_id,
                ),
            }
            .into(),

            StepType::HaltSourceSubnetAtCupHeight => AdminStep {
                logger: self.recovery.logger.clone(),
                ic_admin_cmd: get_halt_subnet_at_cup_height_command(
                    &self.recovery.admin_helper,
                    self.params.source_subnet_id,
                    &self.params.pub_key,
                ),
            }
            .into(),

            StepType::RerouteCanisterRanges => AdminStep {
                logger: self.recovery.logger.clone(),
                ic_admin_cmd: get_propose_to_reroute_canister_ranges_command(
                    &self.recovery.admin_helper,
                    &self.params.canister_id_ranges_to_move,
                    self.params.source_subnet_id,
                    self.params.destination_subnet_id,
                ),
            }
            .into(),

            StepType::DownloadStateFromSourceSubnet => {
                let Some(node_ip) = self.params.download_node_source else {
                    return Err(RecoveryError::StepSkipped);
                };

                self.recovery
                    .get_download_state_step(
                        node_ip,
                        self.params.pub_key.is_some(),
                        self.params.keep_downloaded_state == Some(true),
                        /*additional_excludes=*/
                        vec!["orchestrator", "ic_consensus_pool", IC_REGISTRY_LOCAL_STORE],
                    )
                    .into()
            }
            StepType::CopyDir => CopyWorkDirStep {
                from: self.work_dir(TargetSubnet::Source),
                to: self.work_dir(TargetSubnet::Destination),
                logger: self.recovery.logger.clone(),
            }
            .into(),

            StepType::SplitOutSourceState => self.split_state_step(TargetSubnet::Source).into(),
            StepType::SplitOutDestinationState => {
                self.split_state_step(TargetSubnet::Destination).into()
            }

            StepType::ProposeCupForSourceSubnet => self.propose_cup(TargetSubnet::Source)?.into(),
            StepType::UploadStateToSourceSubnet => {
                self.upload_and_restart_step(TargetSubnet::Source)?.into()
            }
            StepType::ProposeCupForDestinationSubnet => {
                self.propose_cup(TargetSubnet::Destination)?.into()
            }
            StepType::UploadStateToDestinationSubnet => self
                .upload_and_restart_step(TargetSubnet::Destination)?
                .into(),
            StepType::WaitForCUPOnSourceSubnet => {
                self.wait_for_cup_step(TargetSubnet::Source)?.into()
            }
            StepType::WaitForCUPOnDestinationSubnet => {
                self.wait_for_cup_step(TargetSubnet::Destination)?.into()
            }
            StepType::UnhaltSourceSubnet => self.unhalt(TargetSubnet::Source).into(),
            StepType::UnhaltDestinationSubnet => self.unhalt(TargetSubnet::Destination).into(),

            StepType::CompleteCanisterMigration => AdminStep {
                logger: self.recovery.logger.clone(),
                ic_admin_cmd: get_propose_to_complete_canister_migration_command(
                    &self.recovery.admin_helper,
                    &self.params.canister_id_ranges_to_move,
                    self.params.source_subnet_id,
                    self.params.destination_subnet_id,
                ),
            }
            .into(),

            StepType::Cleanup => self.recovery.get_cleanup_step().into(),
            StepType::ComputeExpectedManifestsStep => ComputeExpectedManifestsStep {
                recovery_dir: self.recovery.recovery_dir.clone(),
                state_tool_helper: self.state_tool_helper.clone(),
                source_subnet_id: self.params.source_subnet_id,
                destination_subnet_id: self.params.destination_subnet_id,
                canister_id_ranges_to_move: self.params.canister_id_ranges_to_move.clone(),
            }
            .into(),
        };

        Ok(step)
    }
}

impl Iterator for SubnetSplitting {
    type Item = (StepType, Box<dyn Step>);
    fn next(&mut self) -> Option<Self::Item> {
        self.next_step()
    }
}

impl HasRecoveryState for SubnetSplitting {
    type StepType = StepType;
    type SubcommandArgsType = SubnetSplittingArgs;

    fn get_next_step(&self) -> Option<Self::StepType> {
        self.params.next_step
    }

    fn get_state(&self) -> RecoveryResult<RecoveryState<Self::SubcommandArgsType>> {
        Ok(RecoveryState {
            recovery_args: self.recovery_args.clone(),
            neuron_args: self.neuron_args.clone(),
            subcommand_args: self.params.clone(),
        })
    }
}

fn print_url_and_ask_for_confirmation(
    logger: &Logger,
    url: String,
    text_to_display: impl std::fmt::Display,
) {
    match Url::parse(&url) {
        Ok(url) => {
            info!(logger, "{}", text_to_display);
            info!(logger, "{}", url);
            wait_for_confirmation(logger);
        }
        Err(err) => {
            warn!(logger, "Failed to parse url {}: {}", url, err);
        }
    }
}
