use crate::canister_api::CanisterApi;
use crate::pb::hash_to_hex_string;
use crate::pb::v1::update_allowed_principals_response::AllowedPrincipals;
use crate::pb::v1::{
    add_wasm_response, update_allowed_principals_response, AddWasmRequest, AddWasmResponse,
    DeployNewSnsRequest, DeployNewSnsResponse, DeployedSns, GetAllowedPrincipalsResponse,
    GetNextSnsVersionRequest, GetNextSnsVersionResponse, GetSnsSubnetIdsResponse, GetWasmRequest,
    GetWasmResponse, ListDeployedSnsesRequest, ListDeployedSnsesResponse, SnsCanisterIds,
    SnsCanisterType, SnsVersion, SnsWasm, SnsWasmError, SnsWasmStableIndex, StableCanisterState,
    UpdateAllowedPrincipalsRequest, UpdateAllowedPrincipalsResponse, UpdateSnsSubnetListRequest,
    UpdateSnsSubnetListResponse,
};
use crate::stable_memory::SnsWasmStableMemory;
use candid::Encode;
#[cfg(target_arch = "wasm32")]
use dfn_core::println;
use ic_base_types::{CanisterId, PrincipalId};
use ic_cdk::api::stable::StableMemory;
use ic_nns_constants::{GOVERNANCE_CANISTER_ID, ROOT_CANISTER_ID};
use ic_sns_governance::pb::v1::governance::Version;
use ic_sns_init::SnsCanisterInitPayloads;
use ic_types::{Cycles, SubnetId};
use maplit::hashmap;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::TryInto;
use std::iter::zip;
use std::str::FromStr;
use std::thread::LocalKey;

const LOG_PREFIX: &str = "[SNS-WASM] ";
const DEFAULT_ALLOWED_PRINCIPALS: [&str; 1] = ["b6ps5-uiaaa-aaaal-abhta-cai"];

impl From<SnsCanisterIds> for DeployedSns {
    fn from(src: SnsCanisterIds) -> Self {
        Self {
            root_canister_id: src.root,
            governance_canister_id: src.governance,
            ledger_canister_id: src.ledger,
            swap_canister_id: src.swap,
            index_canister_id: src.index,
        }
    }
}

/// The struct that implements the public API of the canister
#[derive(Clone, Default)]
pub struct SnsWasmCanister<M: StableMemory + Clone + Default>
where
    SnsWasmCanister<M>: From<StableCanisterState>,
{
    /// A map from WASM hash to the index of this WASM in stable memory
    pub wasm_indexes: BTreeMap<[u8; 32], SnsWasmStableIndex>,
    /// Allowed subnets for SNS's to be installed
    pub sns_subnet_ids: Vec<SubnetId>,
    /// Stored deployed_sns instances
    pub deployed_sns_list: Vec<DeployedSns>,
    /// Specifies the upgrade path for SNS instances
    pub upgrade_path: UpgradePath,
    /// Provides convenient access to stable memory
    pub stable_memory: SnsWasmStableMemory<M>,
    /// If true, updates (e.g. add_wasm) can only be made by NNS Governance
    /// (via proposal execution), otherwise updates can be made by any caller
    pub access_controls_enabled: bool,
    /// List of principals that are allowed to deploy an SNS
    pub allowed_principals: Vec<PrincipalId>,
}
const ONE_TRILLION: u64 = 1_000_000_000_000;
const ONE_BILLION: u64 = 1_000_000_000;

const SNS_CREATION_FEE: u64 = 50 * ONE_TRILLION;
const INITIAL_CANISTER_CREATION_CYCLES: u64 = 500 * ONE_BILLION;

/// Internal implementation to give the wasms we explicitly handle a name (instead of Vec<u8>) for
/// safer handling in our internal logic.  This is not intended to be persisted outside of method logic
struct SnsWasmsForDeploy {
    root: Vec<u8>,
    governance: Vec<u8>,
    ledger: Vec<u8>,
    swap: Vec<u8>,
    index: Vec<u8>,
}

/// Helper function to create a DeployError::Validation(ValidationDeployError {})
/// Directly returns the error (unlike other two helpers)
fn validation_deploy_error(message: String) -> DeployError {
    DeployError::Validation(ValidationDeployError { message })
}

/// Helper function to create a DeployError::Reversible(ReversibleDeployError {})
/// Returns a function that takes an error message and returns the DeployError
fn reversible_deploy_error(
    canisters_to_delete: SnsCanisterIds,
    subnet: SubnetId,
) -> impl Fn(String) -> DeployError {
    move |message| {
        DeployError::Reversible(RerversibleDeployError {
            message,
            canisters_to_delete: Some(canisters_to_delete),
            subnet: Some(subnet),
        })
    }
}

/// Helper function to create a DeployError::Irreversible(IrreversibleDeployError {})
/// Returns a function that takes the error message and returns the DeployError
fn irreversible_depoy_error(
    canisters_created: SnsCanisterIds,
    subnet: SubnetId,
) -> impl Fn(String) -> DeployError {
    move |message| {
        DeployError::Irreversible(IrreversibleDeployError {
            message,
            canisters_created,
            subnet,
        })
    }
}

/// Concatenates error messages from a vector of Result<(), String>, if one or more errors is found
fn join_errors_or_ok(results: Vec<Result<(), String>>) -> Result<(), String> {
    if results.iter().any(|r| r.is_err()) {
        Err(results
            .into_iter()
            .flat_map(|result| match result {
                Ok(_) => None,
                Err(e) => Some(e),
            })
            .collect::<Vec<_>>()
            .join("\n"))
    } else {
        Ok(())
    }
}

enum DeployError {
    Validation(ValidationDeployError),
    Reversible(RerversibleDeployError),
    Irreversible(IrreversibleDeployError),
}

/// Error in preconditions
struct ValidationDeployError {
    /// The error message to be returned externally
    message: String,
}

/// Struct representing an error that can be cleaned up
#[derive(Clone)]
struct RerversibleDeployError {
    /// The error message to be returned externally
    message: String,
    /// Canisters created that need to be cleaned up
    canisters_to_delete: Option<SnsCanisterIds>,
    /// Subnet where canister_to_delete live (which is returned when cleanup fails)
    subnet: Option<SubnetId>,
}

/// Struct representing an error that cannot be recovered from (internally)
struct IrreversibleDeployError {
    /// The error message to be returned externally
    message: String,
    /// Canisters created that cannot be cleaned up (when failing set_controllers step)
    canisters_created: SnsCanisterIds,
    /// Subnet where canisters_created that cannot be cleaned up are deployed to
    subnet: SubnetId,
}

impl From<DeployError> for DeployNewSnsResponse {
    fn from(error: DeployError) -> Self {
        match error {
            DeployError::Validation(validation_error) => DeployNewSnsResponse {
                subnet_id: None,
                canisters: None,
                error: Some(SnsWasmError {
                    message: validation_error.message,
                }),
            },
            DeployError::Irreversible(irreversible) => DeployNewSnsResponse {
                canisters: Some(irreversible.canisters_created),
                subnet_id: Some(irreversible.subnet.get()),
                error: Some(SnsWasmError {
                    message: irreversible.message,
                }),
            },
            DeployError::Reversible(_) => {
                panic!("Do not try to use into() for DeployError::Reversible as this should be cleaned up")
            }
        }
    }
}

impl<M: StableMemory + Clone + Default> SnsWasmCanister<M>
where
    SnsWasmCanister<M>: From<StableCanisterState>,
{
    pub fn new() -> Self {
        SnsWasmCanister::<M>::default()
    }

    pub fn set_sns_subnets(&mut self, subnet_ids: Vec<SubnetId>) {
        self.sns_subnet_ids = subnet_ids;
    }

    pub fn set_access_controls_enabled(&mut self, access_controls_enabled: bool) {
        self.access_controls_enabled = access_controls_enabled;
    }

    pub fn set_allowed_principals(&mut self, allowed_principals: Vec<PrincipalId>) {
        self.allowed_principals = allowed_principals;
    }

    /// Initialize stable memory. Should only be called on canister init.
    pub fn initialize_stable_memory(&self) {
        self.stable_memory
            .init()
            .expect("Failed to initialize stable memory")
    }

    /// Returns an Option(SnsWasm) in the GetWasmResponse (a struct with wasm bytecode and the install target)
    pub fn get_wasm(&self, get_wasm_payload: GetWasmRequest) -> GetWasmResponse {
        let hash = vec_to_hash(get_wasm_payload.hash).unwrap();
        GetWasmResponse {
            wasm: self.read_wasm(&hash),
        }
    }

    /// Read a WASM with the given hash from stable memory, if such a WASM exists
    fn read_wasm(&self, hash: &[u8; 32]) -> Option<SnsWasm> {
        self.wasm_indexes
            .get(hash)
            .and_then(|index| self.stable_memory.read_wasm(index.offset, index.size).ok())
    }

    /// Adds a WASM to the canister's storage, validating that the expected hash matches that of the
    /// provided WASM bytecode.
    pub fn add_wasm(&mut self, add_wasm_payload: AddWasmRequest) -> AddWasmResponse {
        let wasm = add_wasm_payload.wasm.expect("Wasm is required");

        let sns_canister_type = match wasm.checked_sns_canister_type() {
            Ok(canister_type) => canister_type,
            Err(message) => {
                println!(
                    "{}add_wasm invalid sns_canister_type: {}",
                    LOG_PREFIX, &message
                );

                return AddWasmResponse {
                    result: Some(add_wasm_response::Result::Error(SnsWasmError { message })),
                };
            }
        };

        let hash = vec_to_hash(add_wasm_payload.hash)
            .expect("Hash provided was not 32 bytes (i.e. [u8;32])");

        if hash != wasm.sha256_hash() {
            return AddWasmResponse {
                result: Some(add_wasm_response::Result::Error(SnsWasmError {
                    message: format!("Invalid Sha256 given for submitted WASM bytes. Provided hash was '{}'  but calculated hash was '{}'",
                                   hash_to_hex_string(&hash), wasm.sha256_string())
                }))
            };
        }

        let result = match self.stable_memory.write_wasm(wasm) {
            Ok((offset, size)) => {
                self.wasm_indexes.insert(
                    hash,
                    SnsWasmStableIndex {
                        hash: hash.to_vec(),
                        offset,
                        size,
                    },
                );

                self.upgrade_path.add_wasm(sns_canister_type, &hash);

                Some(add_wasm_response::Result::Hash(hash.to_vec()))
            }
            Err(e) => {
                println!("{}add_wasm unable to persist WASM: {}", LOG_PREFIX, e);

                Some(add_wasm_response::Result::Error(SnsWasmError {
                    message: format!("Unable to persist WASM: {}", e),
                }))
            }
        };

        AddWasmResponse { result }
    }

    /// Returns a list of Deployed SNS root CanisterId's and the subnet they were deployed to.
    pub fn list_deployed_snses(
        &self,
        _list_sns_payload: ListDeployedSnsesRequest,
    ) -> ListDeployedSnsesResponse {
        ListDeployedSnsesResponse {
            instances: self.deployed_sns_list.clone(),
        }
    }

    /// Deploys a new SNS based on the parameters of the payload
    ///
    /// Main actions that this performs:
    ///   1. Creates the canisters.
    ///   2. Installs SNS root, SNS governance, SNS ledger and SNS index WASMs onto the created canisters.
    ///   3. Fund canisters with cycles
    ///   4. Sets the canisters' controllers:
    ///     a. Root is controlled only by Governance.
    ///     b. Governance is controlled only by Root.
    ///     c. Ledger is controlled only by Root.
    ///     d. Index is controlled only by Root.
    ///
    /// Step 2 requires installation parameters which come from the SnsInitPayload object
    /// included in DeployNewSnsRequest. This adds the created canister IDs to the payloads
    /// so that the SNS canisters know each other's Canister IDs at installation time, which is a
    /// requirement of the SNS deployment.
    ///
    /// In case any operations fail, we try our best to back out of previous changes,
    /// but that is not always possible. Further recovery by the caller may be required in case of failure.
    pub async fn deploy_new_sns(
        thread_safe_sns: &'static LocalKey<RefCell<SnsWasmCanister<M>>>,
        canister_api: &impl CanisterApi,
        deploy_new_sns_payload: DeployNewSnsRequest,
        caller: PrincipalId,
    ) -> DeployNewSnsResponse {
        if !thread_safe_sns.with(|sns_wasm| sns_wasm.borrow().allowed_to_deploy_sns(caller)) {
            return validation_deploy_error(
                "Caller is not in allowed principals list. Cannot deploy an sns.".to_string(),
            )
            .into();
        }
        match Self::do_deploy_new_sns(thread_safe_sns, canister_api, deploy_new_sns_payload).await {
            Ok((subnet_id, canisters)) => DeployNewSnsResponse {
                subnet_id: Some(subnet_id.get()),
                canisters: Some(canisters),
                error: None,
            },
            Err(DeployError::Reversible(reversible)) => {
                // Attempt to clean up after normal failures
                Self::try_cleanup_reversible_deploy_error(canister_api, reversible.clone()).await
            }
            // The rest are conversions as no additional processing is needed
            Err(e) => e.into(),
        }
    }

    pub fn allowed_to_deploy_sns(&self, caller: PrincipalId) -> bool {
        self.allowed_principals.contains(&caller)
    }

    async fn do_deploy_new_sns(
        thread_safe_sns: &'static LocalKey<RefCell<SnsWasmCanister<M>>>,
        canister_api: &impl CanisterApi,
        deploy_new_sns_request: DeployNewSnsRequest,
    ) -> Result<(SubnetId, SnsCanisterIds), DeployError> {
        let sns_init_payload = deploy_new_sns_request
            .sns_init_payload
            // Validate presence
            .ok_or_else(|| "sns_init_payload is a required field".to_string())
            // Validate contents
            .and_then(|init_payload| init_payload.validate().map_err(|e| e.to_string()))
            .map_err(validation_deploy_error)?;

        let subnet_id = thread_safe_sns
            .with(|sns_canister| sns_canister.borrow().get_available_sns_subnet())
            .map_err(validation_deploy_error)?;

        // Ensure we have WASMs available to install before proceeding (avoid unnecessary cleanup)
        let latest_wasms = thread_safe_sns
            .with(|sns_wasms| sns_wasms.borrow().get_latest_version_wasms())
            .map_err(validation_deploy_error)?;

        // If the fee is not present, we fail.
        canister_api
            .message_has_enough_cycles(SNS_CREATION_FEE)
            .map_err(validation_deploy_error)?;

        // After this step, we need to delete the canisters if things fail
        let canisters =
            Self::create_sns_canisters(canister_api, subnet_id, INITIAL_CANISTER_CREATION_CYCLES)
                .await?;
        // This step should never fail unless the step before it fails which would return
        // an error.
        let sns_init_canister_ids = canisters.try_into().expect(
            "This should never happen. Failed to convert SnsCanisterIds into correct type.",
        );

        let latest_version = thread_safe_sns
            .with(|sns_wasms| sns_wasms.borrow().upgrade_path.latest_version.clone());
        // If that works, build the payloads
        let initial_payloads = sns_init_payload
            .build_canister_payloads(
                &sns_init_canister_ids,
                Some(Version {
                    root_wasm_hash: latest_version.root_wasm_hash,
                    governance_wasm_hash: latest_version.governance_wasm_hash,
                    ledger_wasm_hash: latest_version.ledger_wasm_hash,
                    swap_wasm_hash: latest_version.swap_wasm_hash,
                    archive_wasm_hash: latest_version.archive_wasm_hash,
                    index_wasm_hash: latest_version.index_wasm_hash,
                }),
            )
            // NOTE: This error path is not under test, because validate(), called above, should
            // ensure this can never be triggered where validate() would succeed.
            .map_err(|e| {
                reversible_deploy_error(canisters, subnet_id)(format!(
                    "build_canister_payloads failed: {}",
                    e
                ))
            })?;

        // Install the wasms for the canisters.
        Self::install_wasms(canister_api, &canisters, latest_wasms, initial_payloads)
            .await
            .map_err(reversible_deploy_error(canisters, subnet_id))?;

        // At this point, we cannot delete all the canisters necessarily, so we will have to fail
        // and allow some other mechanism to retry setting the correct ownership.
        Self::add_controllers(canister_api, &canisters)
            .await
            .map_err(reversible_deploy_error(canisters, subnet_id))?;

        // We record here because the remaining failures cannot be reversed, so it will be a deployed
        // SNS, but that needs cleanup or extra cycles
        thread_safe_sns.with(|sns_canister| {
            sns_canister
                .borrow_mut()
                .deployed_sns_list
                .push(DeployedSns::from(canisters))
        });

        // We combine the errors of the last two steps because at this point they should both be done
        // even if one fails, since we can no longer back out
        join_errors_or_ok(vec![
            // Accept all remaining cycles and fund the canisters
            Self::fund_canisters(canister_api, &canisters).await,
            // Remove self as the controller
            Self::remove_self_as_controller(canister_api, &canisters).await,
        ])
        .map_err(irreversible_depoy_error(canisters, subnet_id))?;

        Ok((subnet_id, canisters))
    }

    /// Accept remaining cycles in the request, subtract the cycles we've already used, and distribute
    /// the remainder among the canisters
    async fn fund_canisters(
        canister_api: &impl CanisterApi,
        canisters: &SnsCanisterIds,
    ) -> Result<(), String> {
        // Accept the remaining cycles in the request we need to fund the canisters
        let remaining_unaccepted_cycles = canister_api.accept_message_cycles(None).unwrap();
        let cycles_per_canister = remaining_unaccepted_cycles / 5;

        let results = futures::future::join_all(canisters.into_named_tuples().into_iter().map(
            |(label, canister_id)| async move {
                canister_api
                    .send_cycles_to_canister(canister_id, cycles_per_canister)
                    .await
                    .map_err(|e| format!("Could not fund {} canister: {}", label, e))
            },
        ))
        .await;

        join_errors_or_ok(results)
    }

    /// Sets the controllers of the SNS canisters so that Root controls Governance + Ledger, and
    /// Governance controls Root
    async fn add_controllers(
        canister_api: &impl CanisterApi,
        canisters: &SnsCanisterIds,
    ) -> Result<(), String> {
        let this_canister_id = canister_api.local_canister_id().get();

        let set_controllers_results = vec![
            // Set Root as controller of Governance.
            canister_api
                .set_controllers(
                    CanisterId::new(canisters.governance.unwrap()).unwrap(),
                    vec![this_canister_id, canisters.root.unwrap()],
                )
                .await
                .map_err(|e| {
                    format!(
                        "Unable to set Root as Governance canister controller: {}",
                        e
                    )
                }),
            // Set root as controller of Ledger.
            canister_api
                .set_controllers(
                    CanisterId::new(canisters.ledger.unwrap()).unwrap(),
                    vec![this_canister_id, canisters.root.unwrap()],
                )
                .await
                .map_err(|e| format!("Unable to set Root as Ledger canister controller: {}", e)),
            // Set root as controller of Index.
            canister_api
                .set_controllers(
                    CanisterId::new(canisters.index.unwrap()).unwrap(),
                    vec![this_canister_id, canisters.root.unwrap()],
                )
                .await
                .map_err(|e| format!("Unable to set Root as Index canister controller: {}", e)),
            // Set Governance as controller of Root.
            canister_api
                .set_controllers(
                    CanisterId::new(canisters.root.unwrap()).unwrap(),
                    vec![this_canister_id, canisters.governance.unwrap()],
                )
                .await
                .map_err(|e| {
                    format!(
                        "Unable to set Governance as Root canister controller: {}",
                        e
                    )
                }),

            // Set NNS-Root and Swap as controller of Swap
            canister_api
                .set_controllers(
                    CanisterId::new(canisters.swap.unwrap()).unwrap(),
                    vec![this_canister_id, canisters.swap.unwrap(), ROOT_CANISTER_ID.get()],
                )
                .await
                .map_err(|e| {
                    format!(
                        "Unable to set NNS-Root and Swap canister (itself) as Swap canister controller: {}",
                        e
                    )
                }),
        ];

        join_errors_or_ok(set_controllers_results)
    }

    /// Remove the SNS wasm canister as the controller of the canisters
    async fn remove_self_as_controller(
        canister_api: &impl CanisterApi,
        canisters: &SnsCanisterIds,
    ) -> Result<(), String> {
        let set_controllers_results = vec![
            // Removing self, leaving root.
            canister_api
                .set_controllers(
                    CanisterId::new(canisters.governance.unwrap()).unwrap(),
                    vec![canisters.root.unwrap()],
                )
                .await
                .map_err(|e| {
                    format!(
                        "Unable to remove SNS-WASM as Governance's controller: {}",
                        e
                    )
                }),
            // Removing self, leaving root.
            canister_api
                .set_controllers(
                    CanisterId::new(canisters.ledger.unwrap()).unwrap(),
                    vec![canisters.root.unwrap()],
                )
                .await
                .map_err(|e| format!("Unable to remove SNS-WASM as Ledger's controller: {}", e)),
            // Removing self, leaving governance.
            canister_api
                .set_controllers(
                    CanisterId::new(canisters.root.unwrap()).unwrap(),
                    vec![canisters.governance.unwrap()],
                )
                .await
                .map_err(|e| format!("Unable to remove SNS-WASM as Root's controller: {}", e)),
            // Removing self, leaving NNS-Root and Swap
            canister_api
                .set_controllers(
                    CanisterId::new(canisters.swap.unwrap()).unwrap(),
                    vec![canisters.swap.unwrap(), ROOT_CANISTER_ID.get()],
                )
                .await
                .map_err(|e| format!("Unable to remove SNS-WASM as Swap's controller: {}", e)),
            // Removing self, leaving root.
            canister_api
                .set_controllers(
                    CanisterId::new(canisters.index.unwrap()).unwrap(),
                    vec![canisters.root.unwrap()],
                )
                .await
                .map_err(|e| format!("Unable to remove SNS-WASM as Ledger's controller: {}", e)),
        ];

        join_errors_or_ok(set_controllers_results)
    }

    /// Install the SNS Wasms onto the canisters with the specified payloads
    async fn install_wasms(
        canister_api: &impl CanisterApi,
        canisters: &SnsCanisterIds,
        latest_wasms: SnsWasmsForDeploy,
        init_payloads: SnsCanisterInitPayloads,
    ) -> Result<(), String> {
        let results = zip(
            vec!["Root", "Governance", "Ledger", "Swap"],
            futures::future::join_all(vec![
                canister_api.install_wasm(
                    CanisterId::new(canisters.root.unwrap()).unwrap(),
                    latest_wasms.root,
                    Encode!(&init_payloads.root).unwrap(),
                ),
                canister_api.install_wasm(
                    CanisterId::new(canisters.governance.unwrap()).unwrap(),
                    latest_wasms.governance,
                    Encode!(&init_payloads.governance).unwrap(),
                ),
                canister_api.install_wasm(
                    CanisterId::new(canisters.ledger.unwrap()).unwrap(),
                    latest_wasms.ledger,
                    Encode!(&init_payloads.ledger).unwrap(),
                ),
                canister_api.install_wasm(
                    CanisterId::new(canisters.index.unwrap()).unwrap(),
                    latest_wasms.index,
                    Encode!(&init_payloads.index).unwrap(),
                ),
                canister_api.install_wasm(
                    CanisterId::new(canisters.swap.unwrap()).unwrap(),
                    latest_wasms.swap,
                    Encode!(&init_payloads.swap).unwrap(),
                ),
            ])
            .await,
        )
        .into_iter()
        .map(|(label, result)| {
            result.map_err(|e| format!("Error installing {} WASM: {}", label, e))
        })
        .collect();

        join_errors_or_ok(results)
    }

    /// Creates the Canisters for the SNS to be deployed, or returns a ReversibleDeployError
    async fn create_sns_canisters(
        canister_api: &impl CanisterApi,
        subnet_id: SubnetId,
        initial_cycles_per_canister: u64,
    ) -> Result<SnsCanisterIds, DeployError> {
        // Accept enough cycles to simply create the canisters.
        canister_api
            .accept_message_cycles(Some(initial_cycles_per_canister.saturating_mul(5)))
            .map_err(|e| {
                DeployError::Reversible(RerversibleDeployError {
                    message: format!(
                        "Could not accept cycles from request needed to create canisters: {}",
                        e
                    ),
                    canisters_to_delete: None,
                    subnet: None,
                })
            })?;

        let this_canister_id = canister_api.local_canister_id().get();
        let new_canister = || {
            canister_api.create_canister(
                subnet_id,
                this_canister_id,
                Cycles::new(initial_cycles_per_canister.into()),
            )
        };

        // Create these in order instead of join_all to get deterministic ordering for tests
        let canisters_attempted = vec![
            new_canister().await,
            new_canister().await,
            new_canister().await,
            new_canister().await,
            new_canister().await,
        ];
        let canisters_attempted_count = canisters_attempted.len();

        let mut canisters_created = canisters_attempted
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        let canisters_created_count = canisters_created.len();

        if canisters_created_count < canisters_attempted_count {
            let next = |c: &mut Vec<CanisterId>| {
                if !c.is_empty() {
                    Some(c.remove(0).get())
                } else {
                    None
                }
            };
            let canisters_to_delete = SnsCanisterIds {
                root: next(&mut canisters_created),
                governance: next(&mut canisters_created),
                ledger: next(&mut canisters_created),
                swap: next(&mut canisters_created),
                index: next(&mut canisters_created),
            };
            return Err(DeployError::Reversible(RerversibleDeployError {
                message: format!(
                    "Could not create needed canisters.  Only created {} but 5 needed.",
                    canisters_created_count
                ),
                canisters_to_delete: Some(canisters_to_delete),
                subnet: None,
            }));
        }

        Ok(SnsCanisterIds {
            root: Some(canisters_created.remove(0).get()),
            governance: Some(canisters_created.remove(0).get()),
            ledger: Some(canisters_created.remove(0).get()),
            swap: Some(canisters_created.remove(0).get()),
            index: Some(canisters_created.remove(0).get()),
        })
    }

    // Attempt to clean up canisters that were created.
    async fn try_cleanup_reversible_deploy_error(
        canister_api: &impl CanisterApi,
        deploy_error: RerversibleDeployError,
    ) -> DeployNewSnsResponse {
        let success_response = DeployNewSnsResponse {
            subnet_id: None,
            canisters: None,
            error: Some(SnsWasmError {
                message: deploy_error.message.clone(),
            }),
        };
        let named_canister_tuples = match deploy_error.canisters_to_delete {
            None => return success_response,
            Some(canisters) => canisters.into_named_tuples(),
        };

        let results = futures::future::join_all(
            named_canister_tuples
                .into_iter()
                .map(|(label, canister_id)| async move {
                    (label, canister_api.delete_canister(canister_id).await)
                })
                .collect::<Vec<_>>(),
        )
        .await;

        // Map labels together with Option(Result)
        let results = results
            .into_iter()
            .map(|(name, result)| {
                result.map_err(|e| format!("Could not delete {} canister: {}", name, e))
            })
            .collect::<Vec<_>>();

        match join_errors_or_ok(results) {
            Ok(_) => success_response,
            Err(message) => {
                let message = format!(
                    "Failure deploying, and could not finish cleanup.  Some canisters \
                                may not have been deleted. Deployment failure was caused by: '{}' \
                                \n Cleanup failure was caused by: '{}'",
                    deploy_error.message, message
                );
                DeployNewSnsResponse {
                    subnet_id: deploy_error.subnet.map(|s| s.get()),
                    canisters: deploy_error.canisters_to_delete,
                    error: Some(SnsWasmError { message }),
                }
            }
        }
    }

    /// Get an available subnet to create canisters on
    fn get_available_sns_subnet(&self) -> Result<SubnetId, String> {
        // TODO We need a way to find "available" subnets based on SNS deployments (limiting numbers per Subnet)
        if !self.sns_subnet_ids.is_empty() {
            Ok(self.sns_subnet_ids[0])
        } else {
            Err("No SNS Subnet is available".to_string())
        }
    }

    /// Given the SnsVersion of an SNS instance, returns the SnsVersion that this SNS instance
    /// should upgrade to
    pub fn get_next_sns_version(
        &self,
        request: GetNextSnsVersionRequest,
    ) -> GetNextSnsVersionResponse {
        let next_version = request
            .current_version
            .and_then(|sns_version| self.upgrade_path.upgrade_path.get(&sns_version).cloned());

        GetNextSnsVersionResponse { next_version }
    }

    /// Gets the latest/current SNS version in a human-readable format
    pub fn get_latest_sns_version_pretty(&self) -> HashMap<String, String> {
        let version = &self.upgrade_path.latest_version;

        let mut versions_str = HashMap::<String, String>::new();

        versions_str.insert("Root".into(), hex::encode(&version.root_wasm_hash));
        versions_str.insert(
            "Governance".into(),
            hex::encode(&version.governance_wasm_hash),
        );
        versions_str.insert("Ledger".into(), hex::encode(&version.ledger_wasm_hash));
        versions_str.insert("Swap".into(), hex::encode(&version.swap_wasm_hash));
        versions_str.insert(
            "Ledger Archive".into(),
            hex::encode(&version.archive_wasm_hash),
        );

        versions_str
    }

    /// Get the latest version of the WASMs based on the latest SnsVersion
    fn get_latest_version_wasms(&self) -> Result<SnsWasmsForDeploy, String> {
        let version = &self.upgrade_path.latest_version;

        let root = self
            .read_wasm(
                &vec_to_hash(version.root_wasm_hash.clone())
                    .map_err(|_| "No root wasm set for this version.".to_string())?,
            )
            .ok_or_else(|| "Root wasm for this version not found in storage.".to_string())?
            .wasm;

        let governance = self
            .read_wasm(
                &vec_to_hash(version.governance_wasm_hash.clone())
                    .map_err(|_| "No governance wasm set for this version.".to_string())?,
            )
            .ok_or_else(|| "Governance wasm for this version not found in storage.".to_string())?
            .wasm;

        let ledger = self
            .read_wasm(
                &vec_to_hash(version.ledger_wasm_hash.clone())
                    .map_err(|_| "No ledger wasm set for this version.".to_string())?,
            )
            .ok_or_else(|| "Ledger wasm for this version not found in storage.".to_string())?
            .wasm;

        let swap = self
            .read_wasm(
                &vec_to_hash(version.swap_wasm_hash.clone())
                    .map_err(|_| "No swap wasm set for this version.".to_string())?,
            )
            .ok_or_else(|| "Swap wasm for this version not found in storage.".to_string())?
            .wasm;

        let index = self
            .read_wasm(
                &vec_to_hash(version.index_wasm_hash.clone())
                    .map_err(|_| "No index wasm set for this version.".to_string())?,
            )
            .ok_or_else(|| "Index wasm for this version not found in storage.".to_string())?
            .wasm;

        // We do not need this to be set to install, but no upgrade path will be found by the installed
        // SNS if we do not have this as part of the version.
        self.read_wasm(
            &vec_to_hash(version.archive_wasm_hash.clone())
                .map_err(|_| "No archive wasm set for this version.".to_string())?,
        )
        .ok_or_else(|| "Archive wasm for this version not found in storage.".to_string())?;

        Ok(SnsWasmsForDeploy {
            root,
            governance,
            ledger,
            swap,
            index,
        })
    }

    /// Write canister state to stable memory
    pub fn write_state_to_stable_memory(&self) {
        self.stable_memory
            .write_canister_state(self.clone().into())
            .expect("Failed to write canister state from stable memory")
    }

    /// Read canister state from stable memory
    pub fn from_stable_memory() -> Self {
        SnsWasmStableMemory::<M>::default()
            .read_canister_state()
            .expect("Failed to read canister state from stable memory")
            .into()
    }

    /// Update allowed principals list
    pub fn update_allowed_principals(
        &mut self,
        update_allowed_principals_request: UpdateAllowedPrincipalsRequest,
        caller: PrincipalId,
    ) -> UpdateAllowedPrincipalsResponse {
        if caller != GOVERNANCE_CANISTER_ID.into() {
            return UpdateAllowedPrincipalsResponse::error(
                "Only Governance can call update_allowed_principals".to_string(),
            );
        }
        let remove_set: HashSet<PrincipalId> = update_allowed_principals_request
            .removed_principals
            .into_iter()
            .collect();

        let add_set: HashSet<PrincipalId> = update_allowed_principals_request
            .added_principals
            .into_iter()
            .collect();
        let current_set: HashSet<PrincipalId> =
            self.allowed_principals.clone().into_iter().collect();

        self.allowed_principals = current_set
            .union(&add_set)
            .copied()
            .collect::<HashSet<PrincipalId>>()
            .difference(&remove_set)
            .copied()
            .collect();

        UpdateAllowedPrincipalsResponse {
            update_allowed_principals_result: Some(
                update_allowed_principals_response::UpdateAllowedPrincipalsResult::AllowedPrincipals(
                    AllowedPrincipals {
                        allowed_principals: self.allowed_principals.clone(),
                    },
                ),
            ),
        }
    }

    /// Add the principals in `DEFAULT_ALLOWED_PRINCIPALS` to `self.allowed_principals`
    pub fn add_default_allowed_principals(&mut self) {
        let default_principals: HashSet<PrincipalId> = DEFAULT_ALLOWED_PRINCIPALS
            .iter()
            .map(|&p| PrincipalId::from_str(p))
            .filter(|result| result.is_ok())
            .map(|result| result.unwrap())
            .collect();

        let current_set: HashSet<PrincipalId> =
            self.allowed_principals.clone().into_iter().collect();

        self.allowed_principals = current_set.union(&default_principals).copied().collect()
    }

    // Get the list of principals allowed to deploy an sns.
    pub fn get_allowed_principals(&self) -> GetAllowedPrincipalsResponse {
        GetAllowedPrincipalsResponse {
            allowed_principals: self.allowed_principals.clone(),
        }
    }

    /// Add or remove SNS subnet IDs from the list of subnet IDs that SNS instances will be
    /// deployed to
    pub fn update_sns_subnet_list(
        &mut self,
        request: UpdateSnsSubnetListRequest,
    ) -> UpdateSnsSubnetListResponse {
        for subnet_id_to_add in request.sns_subnet_ids_to_add {
            self.sns_subnet_ids.push(SubnetId::new(subnet_id_to_add));
        }

        for subnet_id_to_remove in request.sns_subnet_ids_to_remove {
            self.sns_subnet_ids
                .retain(|id| id != &SubnetId::new(subnet_id_to_remove));
        }

        UpdateSnsSubnetListResponse::ok()
    }

    /// Return the list of SNS subnet IDs that SNS-WASM will deploy SNS instances to
    pub fn get_sns_subnet_ids(&self) -> GetSnsSubnetIdsResponse {
        GetSnsSubnetIdsResponse {
            sns_subnet_ids: self
                .sns_subnet_ids
                .clone()
                .iter()
                .map(|id| id.get())
                .collect(),
        }
    }
}

/// Converts a vector of u8s to array of length 32 (the size of our sha256 hash)
/// or returns an error if wrong length is given
pub fn vec_to_hash(v: Vec<u8>) -> Result<[u8; 32], String> {
    let boxed_slice = v.into_boxed_slice();
    let boxed_array: Box<[u8; 32]> = match boxed_slice.try_into() {
        Ok(hash) => hash,
        Err(original) => {
            return Err(format!(
                "Expected a hash of length {} but it was {}",
                32,
                original.len()
            ))
        }
    };
    Ok(*boxed_array)
}

/// Specifies the upgrade path for SNS instances
#[derive(Clone, Default, Debug, candid::CandidType, candid::Deserialize, PartialEq)]
pub struct UpgradePath {
    /// The latest SNS version. New SNS deployments will deploy the SNS canisters specified by
    /// this version.
    pub latest_version: SnsVersion,

    /// Maps SnsVersions to the SnsVersion that should be upgraded to.
    pub upgrade_path: HashMap<SnsVersion, SnsVersion>,
}

impl UpgradePath {
    pub fn add_wasm(&mut self, canister_type: SnsCanisterType, wasm_hash: &[u8; 32]) {
        let mut new_latest_version = self.latest_version.clone();

        match canister_type {
            SnsCanisterType::Unspecified => panic!("SNS canister type must be non-zero"),
            SnsCanisterType::Root => new_latest_version.root_wasm_hash = wasm_hash.to_vec(),
            SnsCanisterType::Governance => {
                new_latest_version.governance_wasm_hash = wasm_hash.to_vec()
            }
            SnsCanisterType::Ledger => new_latest_version.ledger_wasm_hash = wasm_hash.to_vec(),
            SnsCanisterType::Swap => new_latest_version.swap_wasm_hash = wasm_hash.to_vec(),
            SnsCanisterType::Archive => new_latest_version.archive_wasm_hash = wasm_hash.to_vec(),
            SnsCanisterType::Index => new_latest_version.index_wasm_hash = wasm_hash.to_vec(),
        }

        self.upgrade_path
            .insert(self.latest_version.clone(), new_latest_version.clone());
        self.latest_version = new_latest_version;
    }
}

pub fn assert_unique_canister_ids(sns_1: &SnsCanisterIds, sns_2: &SnsCanisterIds) {
    let mut canister_id_to_name = hashmap! {};
    for (name, canister_id) in [
        ("root 1", sns_1.root.unwrap()),
        ("ledger 1", sns_1.ledger.unwrap()),
        ("governance 1", sns_1.governance.unwrap()),
        ("swap 1", sns_1.swap.unwrap()),
        ("index 1", sns_1.index.unwrap()),
        ("root 2", sns_2.root.unwrap()),
        ("ledger 2", sns_2.ledger.unwrap()),
        ("governance 2", sns_2.governance.unwrap()),
        ("swap 2", sns_2.swap.unwrap()),
        ("index 2", sns_2.index.unwrap()),
    ] {
        use std::collections::hash_map::Entry;
        match canister_id_to_name.entry(canister_id) {
            Entry::Vacant(entry) => {
                // Looking good so far (no panic).
                entry.insert(name);
                continue;
            }
            Entry::Occupied(entry) => {
                panic!(
                    "Canister ID {} not unique: {} vs. {}",
                    canister_id,
                    name,
                    entry.get()
                );
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::canister_stable_memory::TestCanisterStableMemory;
    use async_trait::async_trait;
    use candid::{Decode, Encode};
    use ic_base_types::PrincipalId;
    use ic_crypto_sha::Sha256;
    use ic_icrc1_ledger::InitArgs as LedgerInitArgs;
    use ic_nns_constants::{GOVERNANCE_CANISTER_ID, ROOT_CANISTER_ID};
    use ic_sns_init::pb::v1::SnsInitPayload;
    use ic_test_utilities::types::ids::{canister_test_id, subnet_test_id};
    use maplit::hashset;
    use pretty_assertions::assert_eq;
    use std::sync::{Arc, Mutex};
    use std::vec;

    const CANISTER_CREATION_CYCLES: u64 = INITIAL_CANISTER_CREATION_CYCLES * 5;

    struct TestCanisterApi {
        canisters_created: Arc<Mutex<u64>>,
        // keep track of calls to our mocked methods
        #[allow(clippy::type_complexity)]
        pub install_wasm_calls: Arc<Mutex<Vec<(CanisterId, Vec<u8>, Vec<u8>)>>>,
        #[allow(clippy::type_complexity)]
        pub set_controllers_calls: Arc<Mutex<Vec<(CanisterId, Vec<PrincipalId>)>>>,
        pub cycles_accepted: Arc<Mutex<Vec<u64>>>,
        #[allow(clippy::type_complexity)]
        pub cycles_sent: Arc<Mutex<Vec<(CanisterId, u64)>>>,
        pub canisters_deleted: Arc<Mutex<Vec<CanisterId>>>,
        // How many cycles does the pretend request contain?
        pub cycles_found_in_request: Arc<Mutex<u64>>,
        // Errors that can be thrown at some nth function call
        pub errors_on_create_canister: Arc<Mutex<Vec<Option<String>>>>,
        pub errors_on_set_controller: Arc<Mutex<Vec<Option<String>>>>,
        pub errors_on_delete_canister: Arc<Mutex<Vec<Option<String>>>>,
        pub errors_on_install_wasms: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl CanisterApi for TestCanisterApi {
        fn local_canister_id(&self) -> CanisterId {
            canister_test_id(0)
        }

        async fn create_canister(
            &self,
            _target_subnet: SubnetId,
            _controller_id: PrincipalId,
            _cycles: Cycles,
        ) -> Result<CanisterId, String> {
            let mut errors = self.errors_on_create_canister.lock().unwrap();
            if errors.len() > 0 {
                if let Some(message) = errors.remove(0) {
                    return Err(message);
                }
            }

            let mut data = self.canisters_created.lock().unwrap();
            *data += 1;
            let canister_id = canister_test_id(*data);
            Ok(canister_id)
        }

        async fn delete_canister(&self, canister: CanisterId) -> Result<(), String> {
            self.canisters_deleted.lock().unwrap().push(canister);

            let mut errors = self.errors_on_delete_canister.lock().unwrap();
            if errors.len() > 0 {
                if let Some(message) = errors.remove(0) {
                    return Err(message);
                }
            }

            Ok(())
        }

        async fn install_wasm(
            &self,
            target_canister: CanisterId,
            wasm: Vec<u8>,
            init_payload: Vec<u8>,
        ) -> Result<(), String> {
            self.install_wasm_calls
                .lock()
                .unwrap()
                .push((target_canister, wasm, init_payload));

            let mut errors = self.errors_on_install_wasms.lock().unwrap();
            if errors.len() > 0 {
                if let Some(message) = errors.remove(0) {
                    return Err(message);
                }
            }

            Ok(())
        }

        async fn set_controllers(
            &self,
            canister: CanisterId,
            controllers: Vec<PrincipalId>,
        ) -> Result<(), String> {
            self.set_controllers_calls
                .lock()
                .unwrap()
                .push((canister, controllers));

            let mut errors = self.errors_on_set_controller.lock().unwrap();
            if errors.len() > 0 {
                if let Some(message) = errors.remove(0) {
                    return Err(message);
                }
            }

            Ok(())
        }

        fn message_has_enough_cycles(&self, required_cycles: u64) -> Result<u64, String> {
            let amount = *self.cycles_found_in_request.lock().unwrap();
            if amount < required_cycles {
                return Err(format!(
                    "Not enough cycles in request.  Required: {}. Found: {}",
                    required_cycles, amount
                ));
            }
            Ok(amount)
        }

        async fn send_cycles_to_canister(
            &self,
            target_canister: CanisterId,
            cycles: u64,
        ) -> Result<(), String> {
            self.cycles_sent
                .lock()
                .unwrap()
                .push((target_canister, cycles));
            Ok(())
        }

        fn accept_message_cycles(&self, cycles: Option<u64>) -> Result<u64, String> {
            let cycles = cycles.unwrap_or_else(|| *self.cycles_found_in_request.lock().unwrap());
            self.message_has_enough_cycles(cycles)?;
            self.cycles_accepted.lock().unwrap().push(cycles);

            *self.cycles_found_in_request.lock().unwrap() -= cycles;

            Ok(cycles)
        }
    }

    fn new_canister_api() -> TestCanisterApi {
        TestCanisterApi {
            canisters_created: Arc::new(Mutex::new(0)),
            install_wasm_calls: Arc::new(Mutex::new(vec![])),
            set_controllers_calls: Arc::new(Mutex::new(vec![])),
            cycles_accepted: Arc::new(Mutex::new(vec![])),
            cycles_sent: Arc::new(Mutex::new(vec![])),
            canisters_deleted: Arc::new(Mutex::new(vec![])),
            cycles_found_in_request: Arc::new(Mutex::new(SNS_CREATION_FEE)),
            errors_on_create_canister: Arc::new(Mutex::new(vec![])),
            errors_on_set_controller: Arc::new(Mutex::new(vec![])),
            errors_on_delete_canister: Arc::new(Mutex::new(vec![])),
            errors_on_install_wasms: Arc::new(Mutex::new(vec![])),
        }
    }

    /// Provides a small wasm
    fn smallest_valid_wasm() -> SnsWasm {
        SnsWasm {
            wasm: vec![0, 0x61, 0x73, 0x6D, 1, 0, 0, 0],
            canister_type: i32::from(SnsCanisterType::Governance),
        }
    }

    fn new_wasm_canister() -> SnsWasmCanister<TestCanisterStableMemory> {
        let state = SnsWasmCanister::new();
        state.initialize_stable_memory();
        state
    }

    /// Add some placeholder wasms with different values so we can test
    /// that each value is installed into the correct spot
    fn add_mock_wasms(canister: &mut SnsWasmCanister<TestCanisterStableMemory>) -> SnsVersion {
        let root = SnsWasm {
            wasm: vec![0, 0x61, 0x73, 0x6D, 1, 0, 0, 0],
            canister_type: i32::from(SnsCanisterType::Root),
        };
        let root_wasm_hash = root.sha256_hash().to_vec();
        canister.add_wasm(AddWasmRequest {
            wasm: Some(root),
            hash: root_wasm_hash.clone(),
        });
        let governance = SnsWasm {
            wasm: vec![0, 0x61, 0x73, 0x6D, 1, 0, 0, 1],
            canister_type: i32::from(SnsCanisterType::Governance),
        };
        let governance_wasm_hash = governance.sha256_hash().to_vec();
        canister.add_wasm(AddWasmRequest {
            wasm: Some(governance),
            hash: governance_wasm_hash.clone(),
        });
        let ledger = SnsWasm {
            wasm: vec![0, 0x61, 0x73, 0x6D, 1, 0, 0, 2],
            canister_type: i32::from(SnsCanisterType::Ledger),
        };
        let ledger_wasm_hash = ledger.sha256_hash().to_vec();
        canister.add_wasm(AddWasmRequest {
            wasm: Some(ledger),
            hash: ledger_wasm_hash.clone(),
        });
        let swap = SnsWasm {
            wasm: vec![0, 0x61, 0x73, 0x6D, 1, 0, 0, 3],
            canister_type: i32::from(SnsCanisterType::Swap),
        };
        let swap_wasm_hash = swap.sha256_hash().to_vec();
        canister.add_wasm(AddWasmRequest {
            wasm: Some(swap),
            hash: swap_wasm_hash.clone(),
        });
        let archive = SnsWasm {
            wasm: vec![0, 0x61, 0x73, 0x6D, 1, 0, 0, 4],
            canister_type: i32::from(SnsCanisterType::Archive),
        };
        let archive_wasm_hash = archive.sha256_hash().to_vec();
        canister.add_wasm(AddWasmRequest {
            wasm: Some(archive),
            hash: archive_wasm_hash.clone(),
        });
        let index = SnsWasm {
            wasm: vec![0, 0x61, 0x73, 0x6D, 1, 0, 0, 5],
            canister_type: i32::from(SnsCanisterType::Index),
        };
        let index_wasm_hash = index.sha256_hash().to_vec();
        canister.add_wasm(AddWasmRequest {
            wasm: Some(index),
            hash: index_wasm_hash.clone(),
        });
        SnsVersion {
            root_wasm_hash,
            governance_wasm_hash,
            ledger_wasm_hash,
            swap_wasm_hash,
            archive_wasm_hash,
            index_wasm_hash,
        }
    }

    #[test]
    fn test_update_sns_subnet_list() {
        let mut canister = new_wasm_canister();

        let principal1 = PrincipalId::new_user_test_id(1);
        let principal2 = PrincipalId::new_user_test_id(2);

        // Check that the list of SNS subnet IDs is initially empty
        let response1 = canister.get_sns_subnet_ids();
        assert!(response1.sns_subnet_ids.is_empty());

        // Add a subnet ID and check that it was added
        canister.update_sns_subnet_list(UpdateSnsSubnetListRequest {
            sns_subnet_ids_to_add: vec![principal1],
            sns_subnet_ids_to_remove: vec![],
        });

        let response2 = canister.get_sns_subnet_ids();
        assert_eq!(response2.sns_subnet_ids, vec![principal1]);

        // Remove the first subnet ID and add a new one, and assert that the new subnet ID is the
        // only subnet ID in the SNS subnet list
        canister.update_sns_subnet_list(UpdateSnsSubnetListRequest {
            sns_subnet_ids_to_add: vec![principal2],
            sns_subnet_ids_to_remove: vec![principal1],
        });

        let response3 = canister.get_sns_subnet_ids();
        assert_eq!(response3.sns_subnet_ids, vec![principal2]);
    }

    #[test]
    fn test_api_get_wasm_returns_right_response() {
        let mut canister = new_wasm_canister();

        let wasm = smallest_valid_wasm();
        let expected_hash = Sha256::hash(&wasm.wasm);
        canister.add_wasm(AddWasmRequest {
            wasm: Some(wasm.clone()),
            hash: expected_hash.to_vec(),
        });

        let bad_hash = Sha256::hash("something_else".as_bytes());
        let wasm_response = canister.get_wasm(GetWasmRequest {
            hash: bad_hash.to_vec(),
        });

        // When given non-existent hash, return None
        assert!(wasm_response.wasm.is_none());

        let wasm_response = canister.get_wasm(GetWasmRequest {
            hash: expected_hash.to_vec(),
        });
        // When given valid hash return correct SnsWasm
        assert_eq!(wasm_response.wasm.unwrap(), wasm);
    }

    #[test]
    fn test_api_add_wasm_fails_on_unspecified_canister_type() {
        let mut canister = new_wasm_canister();
        let unspecified_canister_wasm = SnsWasm {
            wasm: vec![0, 0x61, 0x73, 0x6D, 1, 0, 0, 0],
            canister_type: i32::from(SnsCanisterType::Unspecified),
        };

        let response = canister.add_wasm(AddWasmRequest {
            wasm: Some(unspecified_canister_wasm.clone()),
            hash: unspecified_canister_wasm.sha256_hash().to_vec(),
        });

        assert_eq!(
            response,
            AddWasmResponse {
                result: Some(add_wasm_response::Result::Error(SnsWasmError {
                    message: "SnsWasm::canister_type cannot be 'Unspecified' (0).".to_string()
                }))
            }
        )
    }

    #[test]
    fn test_api_add_wasm_fails_on_unsupported_canister_type() {
        let mut canister = new_wasm_canister();
        let invalid_canister_type_wasm = SnsWasm {
            wasm: vec![0, 0x61, 0x73, 0x6D, 1, 0, 0, 0],
            canister_type: 1000,
        };

        let response = canister.add_wasm(AddWasmRequest {
            wasm: Some(invalid_canister_type_wasm.clone()),
            hash: invalid_canister_type_wasm.sha256_hash().to_vec(),
        });

        assert_eq!(
            response,
            AddWasmResponse {
                result: Some(add_wasm_response::Result::Error(
                    SnsWasmError {
                        message: "Invalid value for SnsWasm::canister_type.  See documentation for valid values"
                            .to_string()}))
            } );
    }

    #[test]
    fn test_api_add_wasm_responses() {
        let mut canister = new_wasm_canister();

        let wasm = smallest_valid_wasm();
        let expected_hash = Sha256::hash(&wasm.wasm);
        let bad_hash = Sha256::hash("Something else".as_bytes());
        // First try with incorrect hash
        let failure = canister.add_wasm(AddWasmRequest {
            wasm: Some(wasm.clone()),
            hash: bad_hash.to_vec(),
        });
        assert_eq!(
            failure.result.unwrap(),
            add_wasm_response::Result::Error(SnsWasmError {
                message: format!(
                    "Invalid Sha256 given for submitted WASM bytes. Provided hash was \
                '{}'  but calculated hash was \
                '{}'",
                    hash_to_hex_string(&bad_hash),
                    hash_to_hex_string(&expected_hash),
                )
            })
        );

        let valid_hash = wasm.sha256_hash();
        let success = canister.add_wasm(AddWasmRequest {
            wasm: Some(wasm),
            hash: valid_hash.to_vec(),
        });

        assert_eq!(
            success,
            AddWasmResponse {
                result: Some(add_wasm_response::Result::Hash(valid_hash.to_vec()))
            }
        );
    }

    #[test]
    fn test_update_allowed_principals() {
        let mut canister = new_wasm_canister();

        let update_allowed_principals_response_1 = canister.update_allowed_principals(
            UpdateAllowedPrincipalsRequest {
                added_principals: vec![
                    PrincipalId::new_user_test_id(1),
                    PrincipalId::new_user_test_id(2),
                    PrincipalId::new_user_test_id(3),
                ],
                removed_principals: vec![],
            },
            GOVERNANCE_CANISTER_ID.into(),
        );

        let expected_1 = hashset! {
            PrincipalId::new_user_test_id(1),
            PrincipalId::new_user_test_id(2),
            PrincipalId::new_user_test_id(3),
        };

        match update_allowed_principals_response_1.update_allowed_principals_result {
            Some(update_allowed_principals_response::UpdateAllowedPrincipalsResult::AllowedPrincipals(
                allowed_principals,
            )) => assert_eq!(
                expected_1
                    .symmetric_difference(
                        &allowed_principals
                            .allowed_principals
                            .iter()
                            .copied()
                            .collect()
                    )
                    .collect::<HashSet<&PrincipalId>>(),
                HashSet::new()
            ),
            _ => panic!(
                "Error: update_allowed_principals_response = {:#?}",
                update_allowed_principals_response_1
            ),
        }

        let update_allowed_principals_response_2 = canister.update_allowed_principals(
            UpdateAllowedPrincipalsRequest {
                added_principals: vec![
                    PrincipalId::new_user_test_id(1),
                    PrincipalId::new_user_test_id(4),
                    PrincipalId::new_user_test_id(5),
                ],
                removed_principals: vec![PrincipalId::new_user_test_id(2)],
            },
            GOVERNANCE_CANISTER_ID.into(),
        );

        let expected_2 = hashset! {
            PrincipalId::new_user_test_id(1),
            PrincipalId::new_user_test_id(3),
            PrincipalId::new_user_test_id(4),
            PrincipalId::new_user_test_id(5),
        };

        match update_allowed_principals_response_2.update_allowed_principals_result {
            Some(update_allowed_principals_response::UpdateAllowedPrincipalsResult::AllowedPrincipals(
                allowed_principals,
            )) => assert_eq!(
                expected_2
                    .symmetric_difference(
                        &allowed_principals
                            .allowed_principals
                            .iter()
                            .copied()
                            .collect()
                    )
                    .collect::<HashSet<&PrincipalId>>(),
                HashSet::new()
            ),
            _ => panic!(
                "Error: update_allowed_principals_response = {:#?}",
                update_allowed_principals_response_2
            ),
        }

        assert!(
            expected_2
                .symmetric_difference(&canister.allowed_principals.into_iter().collect())
                .count()
                == 0
        );
    }

    /// Adds Governance and Ledger WASMs and asserts that the upgrade path is updated by
    /// these calls to add_wasm
    #[test]
    fn test_add_wasm_updates_upgrade_path() {
        let mut canister = new_wasm_canister();

        assert_eq!(
            canister.get_next_sns_version(SnsVersion::default().into()),
            GetNextSnsVersionResponse::default()
        );

        let mut wasm = smallest_valid_wasm();

        // Add a Governance WASM
        wasm.canister_type = i32::from(SnsCanisterType::Governance);

        let valid_hash = wasm.sha256_hash();
        canister.add_wasm(AddWasmRequest {
            wasm: Some(wasm.clone()),
            hash: valid_hash.to_vec(),
        });

        // Add a Root WASM
        wasm.canister_type = i32::from(SnsCanisterType::Root);

        canister.add_wasm(AddWasmRequest {
            wasm: Some(wasm.clone()),
            hash: valid_hash.to_vec(),
        });

        // Add a Ledger WASM
        wasm.canister_type = i32::from(SnsCanisterType::Ledger);

        canister.add_wasm(AddWasmRequest {
            wasm: Some(wasm.clone()),
            hash: valid_hash.to_vec(),
        });

        // Add a Swap WASM
        wasm.canister_type = i32::from(SnsCanisterType::Swap);

        canister.add_wasm(AddWasmRequest {
            wasm: Some(wasm.clone()),
            hash: valid_hash.to_vec(),
        });

        // Add an Archive WASM
        wasm.canister_type = i32::from(SnsCanisterType::Archive);

        canister.add_wasm(AddWasmRequest {
            wasm: Some(wasm.clone()),
            hash: valid_hash.to_vec(),
        });

        // Add an Index WASM
        wasm.canister_type = i32::from(SnsCanisterType::Index);

        canister.add_wasm(AddWasmRequest {
            wasm: Some(wasm),
            hash: valid_hash.to_vec(),
        });

        // Assert that the upgrade path was constructed as expected
        let expected_next_sns_version1 = SnsVersion {
            governance_wasm_hash: valid_hash.to_vec(),
            ..Default::default()
        };

        let expected_next_sns_version2 = SnsVersion {
            governance_wasm_hash: valid_hash.to_vec(),
            root_wasm_hash: valid_hash.to_vec(),
            ..Default::default()
        };

        let expected_next_sns_version3 = SnsVersion {
            governance_wasm_hash: valid_hash.to_vec(),
            root_wasm_hash: valid_hash.to_vec(),
            ledger_wasm_hash: valid_hash.to_vec(),
            ..Default::default()
        };

        let expected_next_sns_version4 = SnsVersion {
            governance_wasm_hash: valid_hash.to_vec(),
            root_wasm_hash: valid_hash.to_vec(),
            ledger_wasm_hash: valid_hash.to_vec(),
            swap_wasm_hash: valid_hash.to_vec(),
            ..Default::default()
        };

        let expected_next_sns_version5 = SnsVersion {
            governance_wasm_hash: valid_hash.to_vec(),
            root_wasm_hash: valid_hash.to_vec(),
            ledger_wasm_hash: valid_hash.to_vec(),
            swap_wasm_hash: valid_hash.to_vec(),
            archive_wasm_hash: valid_hash.to_vec(),
            ..Default::default()
        };

        let expected_next_sns_version6 = SnsVersion {
            governance_wasm_hash: valid_hash.to_vec(),
            root_wasm_hash: valid_hash.to_vec(),
            ledger_wasm_hash: valid_hash.to_vec(),
            swap_wasm_hash: valid_hash.to_vec(),
            archive_wasm_hash: valid_hash.to_vec(),
            index_wasm_hash: valid_hash.to_vec(),
        };

        assert_eq!(
            canister.get_next_sns_version(SnsVersion::default().into()),
            expected_next_sns_version1.clone().into()
        );

        assert_eq!(
            canister.get_next_sns_version(expected_next_sns_version1.into()),
            expected_next_sns_version2.clone().into()
        );

        assert_eq!(
            canister.get_next_sns_version(expected_next_sns_version2.into()),
            expected_next_sns_version3.clone().into()
        );

        assert_eq!(
            canister.get_next_sns_version(expected_next_sns_version3.into()),
            expected_next_sns_version4.clone().into()
        );
        assert_eq!(
            canister.get_next_sns_version(expected_next_sns_version4.into()),
            expected_next_sns_version5.clone().into()
        );
        assert_eq!(
            canister.get_next_sns_version(expected_next_sns_version5.into()),
            expected_next_sns_version6.into()
        );
    }

    #[tokio::test]
    async fn test_missing_init_payload() {
        let canister_api = new_canister_api();

        test_deploy_new_sns_request(
            None,
            canister_api,
            Some(subnet_test_id(1)),
            true,
            vec![],
            vec![],
            vec![],
            vec![],
            DeployNewSnsResponse {
                canisters: None,
                subnet_id: None,
                error: Some(SnsWasmError {
                    message: "sns_init_payload is a required field".to_string(),
                }),
            },
        )
        .await;
    }
    #[tokio::test]
    async fn test_invalid_init_payload() {
        let canister_api = new_canister_api();
        let mut payload = SnsInitPayload::with_valid_values_for_testing();
        payload.token_symbol = None;

        test_deploy_new_sns_request(
            Some(payload),
            canister_api,
            Some(subnet_test_id(1)),
            true,
            vec![],
            vec![],
            vec![],
            vec![],
            DeployNewSnsResponse {
                canisters: None,
                subnet_id: None,
                error: Some(SnsWasmError {
                    message: "Error: token-symbol must be specified".to_string(),
                }),
            },
        )
        .await;
    }
    #[tokio::test]
    async fn test_missing_available_subnet() {
        let canister_api = new_canister_api();

        test_deploy_new_sns_request(
            Some(SnsInitPayload::with_valid_values_for_testing()),
            canister_api,
            None,
            true,
            vec![],
            vec![],
            vec![],
            vec![],
            DeployNewSnsResponse {
                canisters: None,
                subnet_id: None,
                error: Some(SnsWasmError {
                    message: "No SNS Subnet is available".to_string(),
                }),
            },
        )
        .await;
    }
    #[tokio::test]
    async fn test_wasms_not_available() {
        let canister_api = new_canister_api();

        test_deploy_new_sns_request(
            Some(SnsInitPayload::with_valid_values_for_testing()),
            canister_api,
            Some(subnet_test_id(1)),
            false, // wasm_available
            vec![],
            vec![],
            vec![],
            vec![],
            DeployNewSnsResponse {
                canisters: None,
                subnet_id: None,
                error: Some(SnsWasmError {
                    message: "No root wasm set for this version.".to_string(),
                }),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_insufficient_cycles_in_request() {
        let mut canister_api = new_canister_api();
        canister_api.cycles_found_in_request = Arc::new(Mutex::new(100000));

        test_deploy_new_sns_request(
            Some(SnsInitPayload::with_valid_values_for_testing()),
            canister_api,
            Some(subnet_test_id(1)),
            true,
            vec![],
            vec![],
            vec![],
            vec![],
            DeployNewSnsResponse {
                subnet_id: None,
                canisters: None,
                error: Some(SnsWasmError {
                    message: format!(
                        "Not enough cycles in request.  Required: {}. Found: {}",
                        SNS_CREATION_FEE, 100000
                    ),
                }),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_failure_if_canisters_cannot_be_created() {
        let canister_api = new_canister_api();
        canister_api
            .errors_on_create_canister
            .lock()
            .unwrap()
            .push(Some("Canister Creation Failed from our test".to_string()));

        let root_id = canister_test_id(1);
        let governance_id = canister_test_id(2);
        let ledger_id = canister_test_id(3);
        let index_id = canister_test_id(4);

        test_deploy_new_sns_request(
            Some(SnsInitPayload::with_valid_values_for_testing()),
            canister_api,
            Some(subnet_test_id(1)),
            true,
            vec![CANISTER_CREATION_CYCLES],
            vec![],
            vec![root_id, governance_id, ledger_id, index_id],
            vec![],
            DeployNewSnsResponse {
                canisters: None,
                subnet_id: None,
                error: Some(SnsWasmError {
                    message: "Could not create needed canisters.  Only created 4 but 5 needed."
                        .to_string(),
                }),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn fail_install_wasms() {
        let canister_api = new_canister_api();
        // don't throw an error until 3rd call to API
        canister_api
            .errors_on_install_wasms
            .lock()
            .unwrap()
            .push(None);
        canister_api
            .errors_on_install_wasms
            .lock()
            .unwrap()
            .push(None);
        canister_api
            .errors_on_install_wasms
            .lock()
            .unwrap()
            .push(Some("Test Failure".to_string()));

        let root_id = canister_test_id(1);
        let governance_id = canister_test_id(2);
        let ledger_id = canister_test_id(3);
        let swap_id = canister_test_id(4);
        let index_id = canister_test_id(5);

        test_deploy_new_sns_request(
            Some(SnsInitPayload::with_valid_values_for_testing()),
            canister_api,
            Some(subnet_test_id(1)),
            true,
            vec![CANISTER_CREATION_CYCLES],
            vec![],
            vec![root_id, governance_id, ledger_id, swap_id, index_id],
            vec![],
            DeployNewSnsResponse {
                subnet_id: None,
                canisters: None,
                error: Some(SnsWasmError {
                    message: "Error installing Ledger WASM: Test Failure".to_string(),
                }),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn fail_add_controllers() {
        let canister_api = new_canister_api();
        canister_api
            .errors_on_set_controller
            .lock()
            .unwrap()
            .push(None);
        canister_api
            .errors_on_set_controller
            .lock()
            .unwrap()
            .push(Some("Set controller fail".to_string()));

        let this_id = canister_test_id(0);

        let root_id = canister_test_id(1);
        let governance_id = canister_test_id(2);
        let ledger_id = canister_test_id(3);
        let swap_id = canister_test_id(4);
        let index_id = canister_test_id(5);

        test_deploy_new_sns_request(
            Some(SnsInitPayload::with_valid_values_for_testing()),
            canister_api,
            Some(subnet_test_id(1)),
            true,
            vec![CANISTER_CREATION_CYCLES],
            vec![],
            vec![root_id, governance_id, ledger_id, swap_id, index_id],
            vec![
                (governance_id, vec![this_id.get(), root_id.get()]),
                (ledger_id, vec![this_id.get(), root_id.get()]),
                (index_id, vec![this_id.get(), root_id.get()]),
                (root_id, vec![this_id.get(), governance_id.get()]),
                (
                    swap_id,
                    vec![this_id.get(), swap_id.get(), ROOT_CANISTER_ID.get()],
                ),
            ],
            DeployNewSnsResponse {
                subnet_id: None,
                canisters: None,
                error: Some(SnsWasmError {
                    message:
                        "Unable to set Root as Ledger canister controller: Set controller fail"
                            .to_string(),
                }),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn fail_remove_self_as_controllers() {
        let canister_api = new_canister_api();
        let mut errors = vec![
            None,
            None,
            None,
            None,
            None,
            Some("Set controller fail".to_string()),
        ];
        canister_api
            .errors_on_set_controller
            .lock()
            .unwrap()
            .append(&mut errors);

        let this_id = canister_test_id(0);

        let root_id = canister_test_id(1);
        let governance_id = canister_test_id(2);
        let ledger_id = canister_test_id(3);
        let swap_id = canister_test_id(4);
        let index_id = canister_test_id(5);

        let sent_cycles = (SNS_CREATION_FEE - CANISTER_CREATION_CYCLES) / 5;

        test_deploy_new_sns_request(
            Some(SnsInitPayload::with_valid_values_for_testing()),
            canister_api,
            Some(subnet_test_id(1)),
            true,
            vec![
                CANISTER_CREATION_CYCLES,
                SNS_CREATION_FEE - CANISTER_CREATION_CYCLES,
            ],
            vec![
                (root_id, sent_cycles),
                (governance_id, sent_cycles),
                (ledger_id, sent_cycles),
                (swap_id, sent_cycles),
                (index_id, sent_cycles),
            ],
            vec![],
            vec![
                (governance_id, vec![this_id.get(), root_id.get()]),
                (ledger_id, vec![this_id.get(), root_id.get()]),
                (index_id, vec![this_id.get(), root_id.get()]),
                (root_id, vec![this_id.get(), governance_id.get()]),
                (
                    swap_id,
                    vec![this_id.get(), swap_id.get(), ROOT_CANISTER_ID.get()],
                ),
                (governance_id, vec![root_id.get()]),
                (ledger_id, vec![root_id.get()]),
                (root_id, vec![governance_id.get()]),
                (swap_id, vec![swap_id.get(), ROOT_CANISTER_ID.get()]),
                (index_id, vec![root_id.get()]),
            ],
            DeployNewSnsResponse {
                subnet_id: Some(subnet_test_id(1).get()),
                canisters: Some(SnsCanisterIds {
                    root: Some(root_id.get()),
                    ledger: Some(ledger_id.get()),
                    governance: Some(governance_id.get()),
                    swap: Some(swap_id.get()),
                    index: Some(index_id.get()),
                }),

                error: Some(SnsWasmError {
                    message:
                        "Unable to remove SNS-WASM as Governance's controller: Set controller fail"
                            .to_string(),
                }),
            },
        )
        .await;
    }

    #[tokio::test]
    async fn fail_cleanup() {
        let canister_api = new_canister_api();
        canister_api
            .errors_on_install_wasms
            .lock()
            .unwrap()
            .push(None);

        canister_api
            .errors_on_install_wasms
            .lock()
            .unwrap()
            .push(Some("Install WASM fail".to_string()));

        canister_api
            .errors_on_delete_canister
            .lock()
            .unwrap()
            .push(Some("Test Failure 1".to_string()));

        canister_api
            .errors_on_delete_canister
            .lock()
            .unwrap()
            .push(Some("Test Failure 2".to_string()));

        let root_id = canister_test_id(1);
        let governance_id = canister_test_id(2);
        let ledger_id = canister_test_id(3);
        let swap_id = canister_test_id(4);
        let index_id = canister_test_id(5);

        test_deploy_new_sns_request(
            Some(SnsInitPayload::with_valid_values_for_testing()),
            canister_api,
            Some(subnet_test_id(1)),
            true,
            vec![CANISTER_CREATION_CYCLES],
            vec![],
            vec![root_id, governance_id, ledger_id, swap_id, index_id],
            vec![],
            DeployNewSnsResponse {
                subnet_id: Some(subnet_test_id(1).get()),
                canisters: Some(SnsCanisterIds {
                    root: Some(root_id.get()),
                    ledger: Some(ledger_id.get()),
                    governance: Some(governance_id.get()),
                    swap: Some(swap_id.get()),
                    index: Some(index_id.get()),
                }),
                error: Some(SnsWasmError {
                    message: "Failure deploying, and could not finish cleanup.  Some canisters may not have been deleted. Deployment failure was caused by: 'Error installing Governance WASM: Install WASM fail' \n Cleanup failure was caused by: 'Could not delete Root canister: Test Failure 1\nCould not delete Governance canister: Test Failure 2'".to_string()
                }),
            },
        )
        .await;
    }

    async fn test_deploy_new_sns_request(
        sns_init_payload: Option<SnsInitPayload>,
        canister_api: TestCanisterApi,
        available_subnet: Option<SubnetId>,
        wasm_available: bool,
        expected_accepted_cycles: Vec<u64>,
        expected_sent_cycles: Vec<(CanisterId, u64)>,
        expected_canisters_destroyed: Vec<CanisterId>,
        expected_set_controllers_calls: Vec<(CanisterId, Vec<PrincipalId>)>,
        expected_response: DeployNewSnsResponse,
    ) {
        thread_local! {
            static CANISTER_WRAPPER: RefCell<SnsWasmCanister<TestCanisterStableMemory>> = RefCell::new(new_wasm_canister()) ;
        }
        CANISTER_WRAPPER.with(|sns_wasm| {
            sns_wasm.borrow_mut().update_allowed_principals(
                UpdateAllowedPrincipalsRequest {
                    added_principals: vec![PrincipalId::new_user_test_id(1)],
                    removed_principals: vec![],
                },
                GOVERNANCE_CANISTER_ID.into(),
            )
        });

        CANISTER_WRAPPER.with(|c| {
            if available_subnet.is_some() {
                c.borrow_mut()
                    .set_sns_subnets(vec![available_subnet.unwrap()]);
            }
            if wasm_available {
                add_mock_wasms(&mut c.borrow_mut());
            }
        });

        let response = SnsWasmCanister::deploy_new_sns(
            &CANISTER_WRAPPER,
            &canister_api,
            DeployNewSnsRequest { sns_init_payload },
            PrincipalId::new_user_test_id(1),
        )
        .await;

        assert_eq!(response, expected_response);

        // Assert that we accepted the cycles
        let cycles_accepted = &*canister_api.cycles_accepted.lock().unwrap();
        assert_eq!(&expected_accepted_cycles, cycles_accepted);

        let cycles_sent = &*canister_api.cycles_sent.lock().unwrap();
        assert_eq!(&expected_sent_cycles, cycles_sent);

        let canisters_destroyed = &*canister_api.canisters_deleted.lock().unwrap();
        assert_eq!(&expected_canisters_destroyed, canisters_destroyed);

        let set_controllers_calls = &*canister_api.set_controllers_calls.lock().unwrap();
        assert_eq!(&expected_set_controllers_calls, set_controllers_calls);
    }

    #[tokio::test]
    async fn test_deploy_new_sns_to_subnet_creates_canisters_and_installs_with_correct_params() {
        thread_local! {
            static CANISTER_WRAPPER: RefCell<SnsWasmCanister<TestCanisterStableMemory>> = RefCell::new(new_wasm_canister()) ;
        }
        CANISTER_WRAPPER.with(|sns_wasm| {
            sns_wasm.borrow_mut().update_allowed_principals(
                UpdateAllowedPrincipalsRequest {
                    added_principals: vec![PrincipalId::new_user_test_id(1)],
                    removed_principals: vec![],
                },
                GOVERNANCE_CANISTER_ID.into(),
            )
        });

        let test_id = subnet_test_id(1);
        let deployed_version = CANISTER_WRAPPER.with(|c| {
            c.borrow_mut().set_sns_subnets(vec![test_id]);
            let sns_version = add_mock_wasms(&mut c.borrow_mut());
            sns_version
        });

        let init_payload = SnsInitPayload::with_valid_values_for_testing();
        let mut canister_api = new_canister_api();
        canister_api.cycles_found_in_request = Arc::new(Mutex::new(SNS_CREATION_FEE + 100));

        let response = SnsWasmCanister::deploy_new_sns(
            &CANISTER_WRAPPER,
            &canister_api,
            DeployNewSnsRequest {
                sns_init_payload: Some(init_payload.clone()),
            },
            PrincipalId::new_user_test_id(1),
        )
        .await;

        let root_id = canister_test_id(1);
        let governance_id = canister_test_id(2);
        let ledger_id = canister_test_id(3);
        let swap_id = canister_test_id(4);
        let index_id = canister_test_id(5);

        assert_eq!(
            response,
            DeployNewSnsResponse {
                subnet_id: Some(test_id.get()),
                canisters: Some(SnsCanisterIds {
                    root: Some(root_id.get()),
                    governance: Some(governance_id.get()),
                    ledger: Some(ledger_id.get()),
                    swap: Some(swap_id.get()),
                    index: Some(index_id.get()),
                }),
                error: None
            }
        );

        let wasms_payloads = init_payload
            .validate()
            .unwrap()
            .build_canister_payloads(
                &SnsCanisterIds {
                    root: Some(root_id.get()),
                    governance: Some(governance_id.get()),
                    ledger: Some(ledger_id.get()),
                    swap: Some(swap_id.get()),
                    index: Some(index_id.get()),
                }
                .try_into()
                .unwrap(),
                Some(Version {
                    root_wasm_hash: deployed_version.root_wasm_hash,
                    governance_wasm_hash: deployed_version.governance_wasm_hash,
                    ledger_wasm_hash: deployed_version.ledger_wasm_hash,
                    swap_wasm_hash: deployed_version.swap_wasm_hash,
                    archive_wasm_hash: deployed_version.archive_wasm_hash,
                    index_wasm_hash: deployed_version.index_wasm_hash,
                }),
            )
            .unwrap();

        // Assert that we accepted the cycles
        let cycles_accepted = &*canister_api.cycles_accepted.lock().unwrap();
        assert_eq!(
            &vec![
                CANISTER_CREATION_CYCLES,
                SNS_CREATION_FEE + 100 - CANISTER_CREATION_CYCLES
            ],
            cycles_accepted
        );

        // We subtract our initial creation fee sent, then send the remainder here
        let fifth_remaining = (SNS_CREATION_FEE + 100 - CANISTER_CREATION_CYCLES) / 5;
        let cycles_sent = &*canister_api.cycles_sent.lock().unwrap();
        assert_eq!(
            &vec![
                (root_id, fifth_remaining),
                (governance_id, fifth_remaining),
                (ledger_id, fifth_remaining),
                (swap_id, fifth_remaining),
                (index_id, fifth_remaining),
            ],
            cycles_sent
        );

        // Now we assert that the expected canisters got the expected wasms with expected init params
        let SnsCanisterInitPayloads {
            root,
            governance,
            ledger,
            ..
        } = wasms_payloads;

        let root_args = canister_api.install_wasm_calls.lock().unwrap().remove(0);
        assert_eq!(
            root_args,
            (
                // root
                root_id,
                vec![0, 97, 115, 109, 1, 0, 0, 0],
                Encode!(&root).unwrap()
            )
        );

        let governance_args = canister_api.install_wasm_calls.lock().unwrap().remove(0);
        assert_eq!(
            governance_args,
            (
                // governance
                governance_id,
                vec![0, 97, 115, 109, 1, 0, 0, 1],
                Encode!(&governance).unwrap()
            )
        );
        // We actually Decode! here because LedgerCanisterInitPayload uses hashset and hashmap
        // which have non-deterministic ordering (and therefore serialization results)
        let (ledger_canister, ledger_wasm, ledger_init_args) =
            canister_api.install_wasm_calls.lock().unwrap().remove(0);
        assert_eq!(ledger_canister, ledger_id);
        assert_eq!(ledger_wasm, vec![0, 97, 115, 109, 1, 0, 0, 2]);
        assert_ledger_init_args_eq(Decode!(&ledger_init_args, LedgerInitArgs).unwrap(), ledger);

        let set_controllers_calls = &*canister_api.set_controllers_calls.lock().unwrap();

        let this_id = canister_test_id(0);
        assert_eq!(
            &vec![
                (governance_id, vec![this_id.get(), root_id.get()]),
                (ledger_id, vec![this_id.get(), root_id.get()]),
                (index_id, vec![this_id.get(), root_id.get()]),
                (root_id, vec![this_id.get(), governance_id.get()]),
                (
                    swap_id,
                    vec![this_id.get(), swap_id.get(), ROOT_CANISTER_ID.get()]
                ),
                (governance_id, vec![root_id.get()]),
                (ledger_id, vec![root_id.get()]),
                (root_id, vec![governance_id.get()]),
                (swap_id, vec![swap_id.get(), ROOT_CANISTER_ID.get()]),
                (index_id, vec![root_id.get()]),
            ],
            set_controllers_calls
        );
    }

    fn assert_ledger_init_args_eq(left: LedgerInitArgs, right: LedgerInitArgs) {
        fn canonicalize(l: LedgerInitArgs) -> LedgerInitArgs {
            let mut l = l;
            l.initial_balances
                .sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap());
            l.metadata
                .sort_by(|(a, _), (b, _)| a.partial_cmp(b).unwrap());
            l
        }

        assert_eq!(canonicalize(left), canonicalize(right));
    }

    #[tokio::test]
    async fn test_deploy_new_sns_records_root_canisters() {
        let test_id = subnet_test_id(1);
        let mut canister_api = new_canister_api();
        canister_api.cycles_found_in_request = Arc::new(Mutex::new(SNS_CREATION_FEE));

        thread_local! {
            static CANISTER_WRAPPER: RefCell<SnsWasmCanister<TestCanisterStableMemory>> = RefCell::new(new_wasm_canister()) ;
        }

        CANISTER_WRAPPER.with(|sns_wasm| {
            sns_wasm.borrow_mut().update_allowed_principals(
                UpdateAllowedPrincipalsRequest {
                    added_principals: vec![PrincipalId::new_user_test_id(1)],
                    removed_principals: vec![],
                },
                GOVERNANCE_CANISTER_ID.into(),
            )
        });

        CANISTER_WRAPPER.with(|c| {
            c.borrow_mut().set_sns_subnets(vec![test_id]);
            add_mock_wasms(&mut c.borrow_mut());
        });

        let sns_1 = SnsWasmCanister::deploy_new_sns(
            &CANISTER_WRAPPER,
            &canister_api,
            DeployNewSnsRequest {
                sns_init_payload: Some(SnsInitPayload::with_valid_values_for_testing()),
            },
            PrincipalId::new_user_test_id(1),
        )
        .await
        .canisters
        .unwrap();

        // Add more cycles so our second call works
        canister_api.cycles_found_in_request = Arc::new(Mutex::new(SNS_CREATION_FEE));
        let response = SnsWasmCanister::deploy_new_sns(
            &CANISTER_WRAPPER,
            &canister_api,
            DeployNewSnsRequest {
                sns_init_payload: Some(SnsInitPayload::with_valid_values_for_testing()),
            },
            PrincipalId::new_user_test_id(1),
        )
        .await;

        let sns_2 = response.canisters.unwrap();

        assert_unique_canister_ids(&sns_1, &sns_2);

        let known_deployments_response = CANISTER_WRAPPER.with(|canister| {
            canister
                .borrow()
                .list_deployed_snses(ListDeployedSnsesRequest {})
        });

        assert_eq!(
            known_deployments_response,
            ListDeployedSnsesResponse {
                instances: vec![DeployedSns::from(sns_1), DeployedSns::from(sns_2),],
            },
        )
    }
}
