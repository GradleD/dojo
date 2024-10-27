//! The migration module contains the logic for migrating the world.
//!
//! A migration is a sequence of steps that are executed in a specific order,
//! based on the [`WorldDiff`] that is computed from the local and remote world.
//!
//! Migrating a world can be sequenced as follows:
//!
//! 1. First the namespaces are synced.
//! 2. Then, all the resources (Contract, Models, Events) are synced, which can consist of:
//!    - Declaring the classes.
//!    - Registering the resources.
//!    - Upgrading the resources.
//! 3. Once resources are synced, the permissions are synced. Permissions can be in different
//!    states:
//!    - For newly registered resources, the permissions are applied.
//!    - For existing resources, the permissions are compared to the onchain state and the necessary
//!      changes are applied.
//! 4. All contracts that are not initialized are initialized, since permissions are applied,
//!    initialization of contracts can mutate resources.

use std::collections::HashMap;
use std::str::FromStr;

use cainome::cairo_serde::{ByteArray, ClassHash, ContractAddress};
use dojo_utils::{Declarer, Deployer, Invoker, TxnConfig};
use dojo_world::config::ProfileConfig;
use dojo_world::contracts::WorldContract;
use dojo_world::diff::{Manifest, ResourceDiff, WorldDiff, WorldStatus};
use dojo_world::local::ResourceLocal;
use dojo_world::remote::ResourceRemote;
use dojo_world::{utils, ResourceType};
use spinoff::Spinner;
use starknet::accounts::ConnectedAccount;
use starknet_crypto::Felt;
use tracing::trace;

pub mod error;
pub use error::MigrationError;

#[derive(Debug)]
pub struct Migration<A>
where
    A: ConnectedAccount + Sync + Send,
{
    diff: WorldDiff,
    world: WorldContract<A>,
    txn_config: TxnConfig,
    profile_config: ProfileConfig,
}

pub enum MigrationUi {
    Spinner(Spinner),
    None,
}

impl MigrationUi {
    pub fn update_text(&mut self, text: &'static str) {
        match self {
            Self::Spinner(s) => s.update_text(text),
            Self::None => (),
        }
    }

    pub fn stop_and_persist(&mut self, symbol: &'static str, text: &'static str) {
        match self {
            Self::Spinner(s) => s.stop_and_persist(symbol, text),
            Self::None => (),
        }
    }
}

impl<A> Migration<A>
where
    A: ConnectedAccount + Sync + Send,
{
    /// Creates a new migration.
    pub fn new(
        diff: WorldDiff,
        world: WorldContract<A>,
        txn_config: TxnConfig,
        profile_config: ProfileConfig,
    ) -> Self {
        Self { diff, world, txn_config, profile_config }
    }

    /// Migrates the world by syncing the namespaces, resources, permissions and initializing the
    /// contracts.
    ///
    /// TODO: find a more elegant way to pass an UI printer to the ops library than a hard coded
    /// spinner.
    pub async fn migrate(
        &self,
        spinner: &mut MigrationUi,
    ) -> Result<Manifest, MigrationError<A::SignError>> {
        spinner.update_text("Deploying world...");
        self.ensure_world().await?;

        if !self.diff.is_synced() {
            spinner.update_text("Syncing resources...");
            self.sync_resources().await?;
        }

        spinner.update_text("Syncing permissions...");
        self.sync_permissions().await?;

        spinner.update_text("Initializing contracts...");
        self.initialize_contracts().await?;

        Ok(Manifest::new(&self.diff))
    }

    /// Returns whether multicall should be used. By default, it is enabled.
    fn do_multicall(&self) -> bool {
        self.profile_config.migration.as_ref().map_or(true, |m| !m.disable_multicall)
    }

    /// For all contracts that are not initialized, initialize them by using the init call arguments
    /// found in the [`ProfileConfig`].
    async fn initialize_contracts(&self) -> Result<(), MigrationError<A::SignError>> {
        let mut invoker = Invoker::new(&self.world.account, self.txn_config.clone());

        let init_call_args = if let Some(init_call_args) = &self.profile_config.init_call_args {
            init_call_args.clone()
        } else {
            HashMap::new()
        };

        for (selector, resource) in &self.diff.resources {
            if resource.resource_type() == ResourceType::Contract {
                let tag = resource.tag();

                let (do_init, init_call_args) = match resource {
                    ResourceDiff::Created(ResourceLocal::Contract(_)) => {
                        (true, init_call_args.get(&tag).clone())
                    }
                    ResourceDiff::Updated(_, ResourceRemote::Contract(contract)) => {
                        (!contract.is_initialized, init_call_args.get(&tag).clone())
                    }
                    ResourceDiff::Synced(_, ResourceRemote::Contract(contract)) => {
                        (!contract.is_initialized, init_call_args.get(&tag).clone())
                    }
                    _ => (false, None),
                };

                if do_init {
                    // Currently, only felts are supported in the init call data.
                    // The injection of class hash and addresses is no longer supported since the
                    // world contains an internal DNS.
                    let args = if let Some(args) = init_call_args {
                        let mut parsed_args = vec![];
                        for arg in args {
                            parsed_args.push(Felt::from_str(arg)?);
                        }
                        parsed_args
                    } else {
                        vec![]
                    };

                    trace!(tag, ?args, "Initializing contract.");

                    invoker.add_call(self.world.init_contract_getcall(&selector, &args));
                }
            }
        }

        if self.do_multicall() {
            invoker.multicall().await?;
        } else {
            invoker.invoke_all_sequentially().await?;
        }

        Ok(())
    }

    /// Syncs the permissions.
    ///
    /// This first version is naive, and only applies the local permissions to the resources, if the
    /// permission is not already set onchain.
    ///
    /// TODO: An other function must be added to sync the remote permissions to the local ones,
    /// and allow the user to reset the permissions onchain to the local ones.
    ///
    /// TODO: for error message, we need the name + namespace (or the tag for non-namespace
    /// resources). Change `DojoSelector` with a struct containing the local definition of an
    /// overlay resource, which can contain also writers.
    async fn sync_permissions(&self) -> Result<(), MigrationError<A::SignError>> {
        let mut invoker = Invoker::new(&self.world.account, self.txn_config.clone());

        // Only takes the local permissions that are not already set onchain to apply them.
        for (selector, resource) in &self.diff.resources {
            for pdiff in self.diff.get_writers(*selector).only_local() {
                trace!(
                    target = resource.tag(),
                    grantee_tag = pdiff.tag.unwrap_or_default(),
                    grantee_address = format!("{:#066x}", pdiff.address),
                    "Granting writer permission."
                );

                invoker.add_call(
                    self.world.grant_writer_getcall(&selector, &ContractAddress(pdiff.address)),
                );
            }

            for pdiff in self.diff.get_owners(*selector).only_local() {
                trace!(
                    target = resource.tag(),
                    grantee_tag = pdiff.tag.unwrap_or_default(),
                    grantee_address = format!("{:#066x}", pdiff.address),
                    "Granting owner permission."
                );

                invoker.add_call(
                    self.world.grant_owner_getcall(&selector, &ContractAddress(pdiff.address)),
                );
            }
        }

        if self.do_multicall() {
            invoker.multicall().await?;
        } else {
            invoker.invoke_all_sequentially().await?;
        }

        Ok(())
    }

    /// Syncs the resources by declaring the classes and registering/upgrading the resources.
    async fn sync_resources(&self) -> Result<(), MigrationError<A::SignError>> {
        let mut invoker = Invoker::new(&self.world.account, self.txn_config.clone());
        let mut declarer = Declarer::new(&self.world.account, self.txn_config.clone());

        // Namespaces must be synced first, since contracts, models and events are namespaced.
        self.namespaces_getcalls(&mut invoker).await?;

        for (_, resource) in &self.diff.resources {
            match resource.resource_type() {
                ResourceType::Contract => {
                    self.contracts_getcalls(resource, &mut invoker, &mut declarer).await?
                }
                ResourceType::Model => {
                    self.models_getcalls(resource, &mut invoker, &mut declarer).await?
                }
                ResourceType::Event => {
                    self.events_getcalls(resource, &mut invoker, &mut declarer).await?
                }
                _ => continue,
            }
        }

        declarer.declare_all().await?;

        if self.do_multicall() {
            invoker.multicall().await?;
        } else {
            invoker.invoke_all_sequentially().await?;
        }

        Ok(())
    }

    /// Returns the calls required to sync the namespaces.
    async fn namespaces_getcalls(
        &self,
        invoker: &mut Invoker<&A>,
    ) -> Result<(), MigrationError<A::SignError>> {
        for namespace_selector in &self.diff.namespaces {
            // TODO: abstract this expect by having a function exposed in the diff.
            let resource =
                self.diff.resources.get(namespace_selector).expect("Namespace not found in diff.");

            if let ResourceDiff::Created(ResourceLocal::Namespace(namespace)) = resource {
                trace!(name = namespace.name, "Registering namespace.");

                invoker.add_call(
                    self.world
                        .register_namespace_getcall(&ByteArray::from_string(&namespace.name)?),
                );
            }
        }

        Ok(())
    }

    /// Returns the calls required to sync the contracts and add the classes to the declarer.
    ///
    /// Currently, classes are cloned to be flattened, this is not ideal but the [`WorldDiff`]
    /// will be required later.
    /// If we could extract the info before syncing the resources, then we could avoid cloning the
    /// classes.
    async fn contracts_getcalls(
        &self,
        resource: &ResourceDiff,
        invoker: &mut Invoker<&A>,
        declarer: &mut Declarer<&A>,
    ) -> Result<(), MigrationError<A::SignError>> {
        let namespace = resource.namespace();
        let ns_bytearray = ByteArray::from_string(&namespace)?;

        if let ResourceDiff::Created(ResourceLocal::Contract(contract)) = resource {
            trace!(
                namespace,
                name = contract.common.name,
                class_hash = format!("{:#066x}", contract.common.class_hash),
                "Registering contract."
            );

            declarer.add_class(
                contract.common.casm_class_hash,
                contract.common.class.clone().flatten()?,
            );

            invoker.add_call(self.world.register_contract_getcall(
                &contract.dojo_selector(),
                &ns_bytearray,
                &ClassHash(contract.common.class_hash),
            ));
        }

        if let ResourceDiff::Updated(
            ResourceLocal::Contract(contract_local),
            ResourceRemote::Contract(_contract_remote),
        ) = resource
        {
            trace!(
                namespace,
                name = contract_local.common.name,
                class_hash = format!("{:#066x}", contract_local.common.class_hash),
                "Upgrading contract."
            );

            declarer.add_class(
                contract_local.common.casm_class_hash,
                contract_local.common.class.clone().flatten()?,
            );

            invoker.add_call(self.world.upgrade_contract_getcall(
                &ns_bytearray,
                &ClassHash(contract_local.common.class_hash),
            ));
        }

        Ok(())
    }

    /// Returns the calls required to sync the models and add the classes to the declarer.
    async fn models_getcalls(
        &self,
        resource: &ResourceDiff,
        invoker: &mut Invoker<&A>,
        declarer: &mut Declarer<&A>,
    ) -> Result<(), MigrationError<A::SignError>> {
        let namespace = resource.namespace();
        let ns_bytearray = ByteArray::from_string(&namespace)?;

        if let ResourceDiff::Created(ResourceLocal::Model(model)) = resource {
            trace!(
                namespace,
                name = model.common.name,
                class_hash = format!("{:#066x}", model.common.class_hash),
                "Registering model."
            );

            declarer.add_class(model.common.casm_class_hash, model.common.class.clone().flatten()?);

            invoker.add_call(
                self.world
                    .register_model_getcall(&ns_bytearray, &ClassHash(model.common.class_hash)),
            );
        }

        if let ResourceDiff::Updated(
            ResourceLocal::Model(model_local),
            ResourceRemote::Model(_model_remote),
        ) = resource
        {
            trace!(
                namespace,
                name = model_local.common.name,
                class_hash = format!("{:#066x}", model_local.common.class_hash),
                "Upgrading model."
            );

            declarer.add_class(
                model_local.common.casm_class_hash,
                model_local.common.class.clone().flatten()?,
            );

            invoker.add_call(
                self.world.upgrade_model_getcall(
                    &ns_bytearray,
                    &ClassHash(model_local.common.class_hash),
                ),
            );
        }

        Ok(())
    }

    /// Returns the calls required to sync the events and add the classes to the declarer.
    async fn events_getcalls(
        &self,
        resource: &ResourceDiff,
        invoker: &mut Invoker<&A>,
        declarer: &mut Declarer<&A>,
    ) -> Result<(), MigrationError<A::SignError>> {
        let namespace = resource.namespace();
        let ns_bytearray = ByteArray::from_string(&namespace)?;

        if let ResourceDiff::Created(ResourceLocal::Event(event)) = resource {
            trace!(
                namespace,
                name = event.common.name,
                class_hash = format!("{:#066x}", event.common.class_hash),
                "Registering event."
            );

            declarer.add_class(event.common.casm_class_hash, event.common.class.clone().flatten()?);

            invoker.add_call(
                self.world
                    .register_event_getcall(&ns_bytearray, &ClassHash(event.common.class_hash)),
            );
        }

        if let ResourceDiff::Updated(
            ResourceLocal::Event(event_local),
            ResourceRemote::Event(_event_remote),
        ) = resource
        {
            trace!(
                namespace,
                name = event_local.common.name,
                class_hash = format!("{:#066x}", event_local.common.class_hash),
                "Upgrading event."
            );

            declarer.add_class(
                event_local.common.casm_class_hash,
                event_local.common.class.clone().flatten()?,
            );

            invoker.add_call(
                self.world.upgrade_event_getcall(
                    &ns_bytearray,
                    &ClassHash(event_local.common.class_hash),
                ),
            );
        }

        Ok(())
    }

    /// Ensures the world is declared and deployed if necessary.
    async fn ensure_world(&self) -> Result<(), MigrationError<A::SignError>> {
        match &self.diff.world_info.status {
            WorldStatus::Synced => return Ok(()),
            WorldStatus::NotDeployed => {
                trace!("Deploying the first world.");

                Declarer::declare(
                    self.diff.world_info.casm_class_hash,
                    self.diff.world_info.class.clone().flatten()?,
                    &self.world.account,
                    &self.txn_config,
                )
                .await?;

                let deployer = Deployer::new(&self.world.account, self.txn_config.clone());

                deployer
                    .deploy_via_udc(
                        self.diff.world_info.class_hash,
                        utils::world_salt(&self.profile_config.world.seed)?,
                        &[self.diff.world_info.class_hash],
                        Felt::ZERO,
                    )
                    .await?;
            }
            WorldStatus::NewVersion => {
                trace!("Upgrading the world.");

                Declarer::declare(
                    self.diff.world_info.casm_class_hash,
                    self.diff.world_info.class.clone().flatten()?,
                    &self.world.account,
                    &self.txn_config,
                )
                .await?;

                let mut invoker = Invoker::new(&self.world.account, self.txn_config.clone());

                invoker.add_call(
                    self.world.upgrade_getcall(&ClassHash(self.diff.world_info.class_hash)),
                );

                invoker.multicall().await?;
            }
        };

        Ok(())
    }
}
