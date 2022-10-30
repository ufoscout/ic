/// The SNS-WASM canister state that is persisted to stable memory on pre-upgrade and read on
/// post-upgrade.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct StableCanisterState {
    #[prost(message, repeated, tag = "1")]
    pub wasm_indexes: ::prost::alloc::vec::Vec<SnsWasmStableIndex>,
    #[prost(message, repeated, tag = "2")]
    pub sns_subnet_ids: ::prost::alloc::vec::Vec<::ic_base_types::PrincipalId>,
    #[prost(message, repeated, tag = "3")]
    pub deployed_sns_list: ::prost::alloc::vec::Vec<DeployedSns>,
    #[prost(message, optional, tag = "4")]
    pub upgrade_path: ::core::option::Option<UpgradePath>,
    #[prost(bool, tag = "5")]
    pub access_controls_enabled: bool,
    #[prost(message, repeated, tag = "6")]
    pub allowed_principals: ::prost::alloc::vec::Vec<::ic_base_types::PrincipalId>,
}
/// Details the offset and size of a WASM binary in stable memory and the hash of this binary.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct SnsWasmStableIndex {
    #[prost(bytes = "vec", tag = "1")]
    pub hash: ::prost::alloc::vec::Vec<u8>,
    #[prost(uint32, tag = "2")]
    pub offset: u32,
    #[prost(uint32, tag = "3")]
    pub size: u32,
}
/// Specifies the upgrade path for SNS instances.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct UpgradePath {
    /// The latest SNS version. New SNS deployments will deploy the SNS canisters specified by
    /// this version.
    #[prost(message, optional, tag = "1")]
    pub latest_version: ::core::option::Option<SnsVersion>,
    /// Maps SnsVersions to the SnsVersion that it should be upgraded to.
    #[prost(message, repeated, tag = "2")]
    pub upgrade_path: ::prost::alloc::vec::Vec<SnsUpgrade>,
}
/// Maps an SnsVersion to the SnsVersion that it should be upgraded to.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct SnsUpgrade {
    #[prost(message, optional, tag = "1")]
    pub current_version: ::core::option::Option<SnsVersion>,
    #[prost(message, optional, tag = "2")]
    pub next_version: ::core::option::Option<SnsVersion>,
}
/// The representation of a WASM along with its target canister type.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct SnsWasm {
    #[prost(bytes = "vec", tag = "1")]
    pub wasm: ::prost::alloc::vec::Vec<u8>,
    #[prost(enumeration = "SnsCanisterType", tag = "2")]
    pub canister_type: i32,
}
/// The error response returned in response objects on failed or partially failed operations.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct SnsWasmError {
    /// The message returned by the canister on errors.
    #[prost(string, tag = "1")]
    pub message: ::prost::alloc::string::String,
}
/// The payload for the add_wasm endpoint, which takes an SnsWasm along with the hash of the wasm bytes.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct AddWasmRequest {
    #[prost(message, optional, tag = "1")]
    pub wasm: ::core::option::Option<SnsWasm>,
    #[prost(bytes = "vec", tag = "2")]
    pub hash: ::prost::alloc::vec::Vec<u8>,
}
/// The response from add_wasm, which is either Ok or Error.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct AddWasmResponse {
    #[prost(oneof = "add_wasm_response::Result", tags = "1, 2")]
    pub result: ::core::option::Option<add_wasm_response::Result>,
}
/// Nested message and enum types in `AddWasmResponse`.
pub mod add_wasm_response {
    #[derive(
        candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Oneof,
    )]
    pub enum Result {
        /// The hash of the wasm that was added.
        #[prost(bytes, tag = "1")]
        Hash(::prost::alloc::vec::Vec<u8>),
        /// Error when request fails.
        #[prost(message, tag = "2")]
        Error(super::SnsWasmError),
    }
}
/// The argument for get_wasm, which consists of the WASM hash to be retrieved.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct GetWasmRequest {
    #[prost(bytes = "vec", tag = "1")]
    pub hash: ::prost::alloc::vec::Vec<u8>,
}
/// The response for get_wasm, which returns a WASM if it is found, or None.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct GetWasmResponse {
    #[prost(message, optional, tag = "1")]
    pub wasm: ::core::option::Option<SnsWasm>,
}
/// Payload to deploy a new SNS.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct DeployNewSnsRequest {
    /// The initial payload to initialize the SNS with.
    #[prost(message, optional, tag = "1")]
    pub sns_init_payload: ::core::option::Option<::ic_sns_init::pb::v1::SnsInitPayload>,
}
/// The response to creating a new SNS.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct DeployNewSnsResponse {
    /// The subnet the SNS was deployed to.
    #[prost(message, optional, tag = "1")]
    pub subnet_id: ::core::option::Option<::ic_base_types::PrincipalId>,
    /// CanisterIds of canisters created by deploy_new_sns.
    #[prost(message, optional, tag = "2")]
    pub canisters: ::core::option::Option<SnsCanisterIds>,
    /// Error when the request fails.
    #[prost(message, optional, tag = "3")]
    pub error: ::core::option::Option<SnsWasmError>,
}
/// The CanisterIds of the SNS canisters that are created.
#[derive(
    candid::CandidType,
    candid::Deserialize,
    serde::Serialize,
    Copy,
    Clone,
    PartialEq,
    ::prost::Message,
)]
pub struct SnsCanisterIds {
    /// PrincipalId of the root canister.
    #[prost(message, optional, tag = "1")]
    pub root: ::core::option::Option<::ic_base_types::PrincipalId>,
    /// PrincipalId of the ledger canister.
    #[prost(message, optional, tag = "2")]
    pub ledger: ::core::option::Option<::ic_base_types::PrincipalId>,
    /// PrincipalId of the governance canister.
    #[prost(message, optional, tag = "3")]
    pub governance: ::core::option::Option<::ic_base_types::PrincipalId>,
    /// PrincipalId of the swap canister.
    #[prost(message, optional, tag = "4")]
    pub swap: ::core::option::Option<::ic_base_types::PrincipalId>,
    /// PrincipalId of the index canister.
    #[prost(message, optional, tag = "5")]
    pub index: ::core::option::Option<::ic_base_types::PrincipalId>,
}
/// Message to list deployed sns instances.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct ListDeployedSnsesRequest {}
/// Response to list_deployed_snses.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct ListDeployedSnsesResponse {
    /// The deployed instances.
    #[prost(message, repeated, tag = "1")]
    pub instances: ::prost::alloc::vec::Vec<DeployedSns>,
}
/// An SNS deployed by this canister (i.e. the sns-wasm canister).
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct DeployedSns {
    /// ID of the various canisters that were originally created in an SNS.
    #[prost(message, optional, tag = "1")]
    pub root_canister_id: ::core::option::Option<::ic_base_types::PrincipalId>,
    #[prost(message, optional, tag = "2")]
    pub governance_canister_id: ::core::option::Option<::ic_base_types::PrincipalId>,
    #[prost(message, optional, tag = "3")]
    pub ledger_canister_id: ::core::option::Option<::ic_base_types::PrincipalId>,
    #[prost(message, optional, tag = "4")]
    pub swap_canister_id: ::core::option::Option<::ic_base_types::PrincipalId>,
    #[prost(message, optional, tag = "5")]
    pub index_canister_id: ::core::option::Option<::ic_base_types::PrincipalId>,
}
/// Specifies the version of an SNS.
#[derive(
    candid::CandidType,
    candid::Deserialize,
    serde::Serialize,
    Eq,
    Hash,
    Clone,
    PartialEq,
    ::prost::Message,
)]
pub struct SnsVersion {
    /// The hash of the Root canister WASM.
    #[prost(bytes = "vec", tag = "1")]
    pub root_wasm_hash: ::prost::alloc::vec::Vec<u8>,
    /// The hash of the Governance canister WASM.
    #[prost(bytes = "vec", tag = "2")]
    pub governance_wasm_hash: ::prost::alloc::vec::Vec<u8>,
    /// The hash of the Ledger canister WASM.
    #[prost(bytes = "vec", tag = "3")]
    pub ledger_wasm_hash: ::prost::alloc::vec::Vec<u8>,
    /// The hash of the Swap canister WASM.
    #[prost(bytes = "vec", tag = "4")]
    pub swap_wasm_hash: ::prost::alloc::vec::Vec<u8>,
    /// The hash of the Ledger Archive canister WASM.
    #[prost(bytes = "vec", tag = "5")]
    pub archive_wasm_hash: ::prost::alloc::vec::Vec<u8>,
    /// The hash of the Index canister WASM.
    #[prost(bytes = "vec", tag = "6")]
    pub index_wasm_hash: ::prost::alloc::vec::Vec<u8>,
}
/// The request type accepted by the get_next_sns_version canister method.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct GetNextSnsVersionRequest {
    #[prost(message, optional, tag = "1")]
    pub current_version: ::core::option::Option<SnsVersion>,
}
/// The response type returned by the get_next_sns_version canister method.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct GetNextSnsVersionResponse {
    #[prost(message, optional, tag = "1")]
    pub next_version: ::core::option::Option<SnsVersion>,
}
/// The request type accepted by update_allowed_principals.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct UpdateAllowedPrincipalsRequest {
    #[prost(message, repeated, tag = "1")]
    pub added_principals: ::prost::alloc::vec::Vec<::ic_base_types::PrincipalId>,
    #[prost(message, repeated, tag = "2")]
    pub removed_principals: ::prost::alloc::vec::Vec<::ic_base_types::PrincipalId>,
}
/// The response type returned by update_allowed_principals.
/// Returns the allowed principals after the update or an error.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct UpdateAllowedPrincipalsResponse {
    #[prost(
        oneof = "update_allowed_principals_response::UpdateAllowedPrincipalsResult",
        tags = "1, 2"
    )]
    pub update_allowed_principals_result:
        ::core::option::Option<update_allowed_principals_response::UpdateAllowedPrincipalsResult>,
}
/// Nested message and enum types in `UpdateAllowedPrincipalsResponse`.
pub mod update_allowed_principals_response {
    #[derive(
        candid::CandidType,
        candid::Deserialize,
        serde::Serialize,
        Clone,
        PartialEq,
        ::prost::Message,
    )]
    pub struct AllowedPrincipals {
        #[prost(message, repeated, tag = "1")]
        pub allowed_principals: ::prost::alloc::vec::Vec<::ic_base_types::PrincipalId>,
    }
    #[derive(
        candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Oneof,
    )]
    pub enum UpdateAllowedPrincipalsResult {
        #[prost(message, tag = "1")]
        Error(super::SnsWasmError),
        #[prost(message, tag = "2")]
        AllowedPrincipals(AllowedPrincipals),
    }
}
/// The request type for get_allowed_principals.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct GetAllowedPrincipalsRequest {}
/// The response type for get_allowed_principals.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct GetAllowedPrincipalsResponse {
    #[prost(message, repeated, tag = "1")]
    pub allowed_principals: ::prost::alloc::vec::Vec<::ic_base_types::PrincipalId>,
}
/// The request type of update_sns_subnet_list, used to add or remove SNS subnet IDs (these are the subnets that
/// SNS instances will be deployed to)
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct UpdateSnsSubnetListRequest {
    #[prost(message, repeated, tag = "1")]
    pub sns_subnet_ids_to_add: ::prost::alloc::vec::Vec<::ic_base_types::PrincipalId>,
    #[prost(message, repeated, tag = "2")]
    pub sns_subnet_ids_to_remove: ::prost::alloc::vec::Vec<::ic_base_types::PrincipalId>,
}
/// The response type of update_sns_subnet_list
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct UpdateSnsSubnetListResponse {
    #[prost(message, optional, tag = "1")]
    pub error: ::core::option::Option<SnsWasmError>,
}
/// The request type of get_sns_subnet_ids. Used to request the list of SNS subnet IDs that SNS-WASM will deploy
/// SNS instances to.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct GetSnsSubnetIdsRequest {}
/// The request type of get_sns_subnet_ids. Used to request the list of SNS subnet IDs that SNS-WASM will deploy
/// SNS instances to.
#[derive(
    candid::CandidType, candid::Deserialize, serde::Serialize, Clone, PartialEq, ::prost::Message,
)]
pub struct GetSnsSubnetIdsResponse {
    #[prost(message, repeated, tag = "1")]
    pub sns_subnet_ids: ::prost::alloc::vec::Vec<::ic_base_types::PrincipalId>,
}
/// The type of canister a particular WASM is intended to be installed on.
#[derive(
    candid::CandidType,
    candid::Deserialize,
    serde::Serialize,
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    ::prost::Enumeration,
)]
#[repr(i32)]
pub enum SnsCanisterType {
    Unspecified = 0,
    /// The type for the root canister.
    Root = 1,
    /// The type for the governance canister.
    Governance = 2,
    /// The type for the ledger canister.
    Ledger = 3,
    /// The type for the swap canister.
    Swap = 4,
    /// The type for the ledger archive canister.
    Archive = 5,
    /// The type for the index canister.
    Index = 6,
}
impl SnsCanisterType {
    /// String value of the enum field names used in the ProtoBuf definition.
    ///
    /// The values are not transformed in any way and thus are considered stable
    /// (if the ProtoBuf definition does not change) and safe for programmatic use.
    pub fn as_str_name(&self) -> &'static str {
        match self {
            SnsCanisterType::Unspecified => "SNS_CANISTER_TYPE_UNSPECIFIED",
            SnsCanisterType::Root => "SNS_CANISTER_TYPE_ROOT",
            SnsCanisterType::Governance => "SNS_CANISTER_TYPE_GOVERNANCE",
            SnsCanisterType::Ledger => "SNS_CANISTER_TYPE_LEDGER",
            SnsCanisterType::Swap => "SNS_CANISTER_TYPE_SWAP",
            SnsCanisterType::Archive => "SNS_CANISTER_TYPE_ARCHIVE",
            SnsCanisterType::Index => "SNS_CANISTER_TYPE_INDEX",
        }
    }
}
