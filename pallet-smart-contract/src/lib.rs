#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::{
    debug, decl_error, decl_event, decl_module, decl_storage, ensure,
    traits::Vec,
    traits::{Currency, ExistenceRequirement::KeepAlive},
};
use frame_system::{self as system, ensure_signed};
use sp_runtime::{traits::SaturatedConversion, DispatchError, DispatchResult};

use substrate_fixed::types::U64F64;
use pallet_tfgrid;
use pallet_tfgrid::types as pallet_tfgrid_types;
use pallet_tft_price;
use pallet_timestamp as timestamp;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod tests;

pub mod types;

pub trait Config: system::Config + pallet_tfgrid::Config + pallet_timestamp::Config {
    type Event: From<Event<Self>> + Into<<Self as frame_system::Config>::Event>;
    type Currency: Currency<Self::AccountId>;
}

pub const CONTRACT_VERSION: u32 = 1;
// Frequency of contract billing in number of blocks
// 600 = every 1 hour
pub const BILLING_FREQUENCY_IN_BLOCKS: u64 = 600;

pub type BalanceOf<T> =
    <<T as Config>::Currency as Currency<<T as system::Config>::AccountId>>::Balance;

decl_event!(
    pub enum Event<T>
    where
        AccountId = <T as frame_system::Config>::AccountId,
    {
        ContractCreated(types::Contract),
        ContractUpdated(types::Contract),
        NodeContractCanceled(u64, u32, u32),
        NameContractCanceled(u64),
        IPsReserved(u64, Vec<pallet_tfgrid_types::PublicIP>),
        IPsFreed(u64, Vec<Vec<u8>>),
        ContractDeployed(u64, AccountId),
        ConsumptionReportReceived(types::Consumption),
        ContractBilled(types::ContractBill),
    }
);

decl_error! {
    /// Error for the smart contract module.
    pub enum Error for Module<T: Config> {
        TwinNotExists,
        NodeNotExists,
        FarmNotExists,
        FarmHasNotEnoughPublicIPs,
        FarmHasNotEnoughPublicIPsFree,
        FailedToReserveIP,
        FailedToFreeIPs,
        ContractNotExists,
        TwinNotAuthorizedToUpdateContract,
        TwinNotAuthorizedToCancelContract,
        NodeNotAuthorizedToDeployContract,
        NodeNotAuthorizedToComputeReport,
        PricingPolicyNotExists,
        ContractIsNotUnique,
        NameExists,
        NameNotValid,
        InvalidContractType,
        TFTPriceValueError
    }
}

decl_storage! {
    trait Store for Module<T: Config> as SmartContractModule {
        pub Contracts get(fn contracts): map hasher(blake2_128_concat) u64 => types::Contract;
        pub ContractBillingInformationByID get(fn contract_billing_information_by_id): map hasher(blake2_128_concat) u64 => types::ContractBillingInformation;
        // ContractIDByNodeIDAndHash is a mapping for a contract ID by supplying a node_id and a deployment_hash
        // this combination makes a deployment for a user / node unique
        pub ContractIDByNodeIDAndHash get(fn node_contract_by_hash): double_map hasher(blake2_128_concat) u32, hasher(blake2_128_concat) Vec<u8> => u64;
        pub NodeContracts get(fn node_contracts): double_map hasher(blake2_128_concat) u32, hasher(blake2_128_concat) types::ContractState => Vec<types::Contract>;
        pub ContractsToBillAt get(fn contract_to_bill_at_block): map hasher(blake2_128_concat) u64 => Vec<u64>;
        pub ContractIDByNameRegistration get(fn contract_id_by_name_registration): map hasher(blake2_128_concat) Vec<u8> => u64;

        // ID maps
        ContractID: u64;
    }
}

decl_module! {
    pub struct Module<T: Config> for enum Call where origin: T::Origin {
        fn deposit_event() = default;

        #[weight = 10]
        fn create_node_contract(origin, node_id: u32, data: Vec<u8>, deployment_hash: Vec<u8>, public_ips: u32){
            let account_id = ensure_signed(origin)?;
            Self::_create_node_contract(account_id, node_id, data, deployment_hash, public_ips)?;
        }

        #[weight = 10]
        fn update_node_contract(origin, contract_id: u64, data: Vec<u8>, deployment_hash: Vec<u8>){
            let account_id = ensure_signed(origin)?;
            Self::_update_node_contract(account_id, contract_id, data, deployment_hash)?;
        }

        #[weight = 10]
        fn cancel_contract(origin, contract_id: u64){
            let account_id = ensure_signed(origin)?;
            Self::_cancel_contract(account_id, contract_id)?;
        }

        #[weight = 10]
        fn add_reports(origin, reports: Vec<types::Consumption>) {
            let account_id = ensure_signed(origin)?;
            Self::_compute_reports(account_id, reports)?;
        }

        #[weight = 10]
        fn create_name_contract(origin, name: Vec<u8>) {
            let account_id = ensure_signed(origin)?;
            Self::_create_name_contract(account_id, name)?;
        }

        fn on_finalize(block: T::BlockNumber) {
            match Self::_bill_contracts_at_block(block) {
                Ok(_) => {
                    debug::info!("types::NodeContract billed successfully at block: {:?}", block);
                },
                Err(err) => {
                    debug::info!("types::NodeContract billed failed at block: {:?} with err {:?}", block, err);
                }
            }
            // clean storage map for billed contracts at block
            let current_block_u64: u64 = block.saturated_into::<u64>();
            ContractsToBillAt::remove(current_block_u64);
        }
    }
}

impl<T: Config> Module<T> {
    pub fn _create_node_contract(
        account_id: T::AccountId,
        node_id: u32,
        deployment_data: Vec<u8>,
        deployment_hash: Vec<u8>,
        public_ips: u32,
    ) -> DispatchResult {
        ensure!(
            pallet_tfgrid::TwinIdByAccountID::<T>::contains_key(&account_id),
            Error::<T>::TwinNotExists
        );
        ensure!(
            pallet_tfgrid::Nodes::contains_key(&node_id),
            Error::<T>::NodeNotExists
        );

        // if the contract with hash and node id exists and it's in any other state then
        // contractState::Deleted then we don't allow the creation of it.
        // If it exists we allow the user to "restore" this contract
        if ContractIDByNodeIDAndHash::contains_key(node_id, &deployment_hash) {
            let contract_id = ContractIDByNodeIDAndHash::get(node_id, &deployment_hash);
            let contract = Contracts::get(contract_id);
            if contract.state != types::ContractState::Deleted {
                return Err(DispatchError::from(Error::<T>::ContractIsNotUnique));
            }
        }

        let mut id = ContractID::get();
        id = id + 1;

        let twin_id = pallet_tfgrid::TwinIdByAccountID::<T>::get(&account_id);

        let node_contract = types::NodeContract {
            node_id,
            deployment_data,
            deployment_hash: deployment_hash.clone(),
            public_ips,
            public_ips_list: Vec::new(),
        };

        let contract = Self::_create_contract(
            twin_id,
            types::ContractData::NodeContract(node_contract.clone()),
        )?;

        ContractIDByNodeIDAndHash::insert(node_id, deployment_hash, id);

        let mut node_contracts = NodeContracts::get(&node_contract.node_id, &contract.state);
        node_contracts.push(contract.clone());
        NodeContracts::insert(&node_contract.node_id, &contract.state, &node_contracts);

        Self::deposit_event(RawEvent::ContractCreated(contract));

        Ok(())
    }

    fn _create_contract(
        twin_id: u32,
        mut contract_type: types::ContractData,
    ) -> Result<types::Contract, DispatchError> {
        let mut id = ContractID::get();
        id = id + 1;

        if let types::ContractData::NodeContract(ref mut nc) = contract_type {
            Self::_reserve_ip(id, nc)?;
        };

        let contract = types::Contract {
            version: CONTRACT_VERSION,
            twin_id,
            contract_id: id,
            state: types::ContractState::Created,
            contract_type,
        };

        let contract_billing_information = types::ContractBillingInformation {
            last_updated: <timestamp::Module<T>>::get().saturated_into::<u64>() / 1000,
            amount_unbilled: 0,
            previous_nu_reported: 0,
        };

        // Start billing frequency loop
        // Will always be block now + frequency
        Self::_reinsert_contract_to_bill(contract.contract_id);

        Contracts::insert(id, &contract);
        ContractID::put(id);
        ContractBillingInformationByID::insert(id, contract_billing_information);
        Ok(contract)
    }

    pub fn _update_node_contract(
        account_id: T::AccountId,
        contract_id: u64,
        deployment_data: Vec<u8>,
        deployment_hash: Vec<u8>,
    ) -> DispatchResult {
        ensure!(
            Contracts::contains_key(contract_id),
            Error::<T>::ContractNotExists
        );

        let mut contract = Contracts::get(contract_id);
        let twin = pallet_tfgrid::Twins::<T>::get(contract.twin_id);
        ensure!(
            twin.account_id == account_id,
            Error::<T>::TwinNotAuthorizedToUpdateContract
        );

        let mut node_contract = Self::get_node_contract(&contract.clone())?;

        // remove and reinsert contract id by node id and hash because that hash can have changed
        ContractIDByNodeIDAndHash::remove(node_contract.node_id, node_contract.deployment_hash);
        ContractIDByNodeIDAndHash::insert(node_contract.node_id, &deployment_hash, contract_id);

        node_contract.deployment_data = deployment_data;
        node_contract.deployment_hash = deployment_hash;

        // override values
        contract.contract_type = types::ContractData::NodeContract(node_contract);

        let state = contract.state.clone();
        Self::_update_contract_state(&mut contract, &state)?;

        Self::deposit_event(RawEvent::ContractUpdated(contract));

        Ok(())
    }

    pub fn _cancel_contract(account_id: T::AccountId, contract_id: u64) -> DispatchResult {
        ensure!(
            Contracts::contains_key(contract_id),
            Error::<T>::ContractNotExists
        );

        let mut contract = Contracts::get(contract_id);
        let twin = pallet_tfgrid::Twins::<T>::get(contract.twin_id);
        ensure!(
            twin.account_id == account_id,
            Error::<T>::TwinNotAuthorizedToCancelContract
        );

        match contract.contract_type.clone() {
            types::ContractData::NodeContract(mut node_contract) => {
                if node_contract.public_ips > 0 {
                    Self::_free_ip(contract_id, &mut node_contract)?
                }

                // remove the contract by hash from storage
                ContractIDByNodeIDAndHash::remove(
                    node_contract.node_id,
                    &node_contract.deployment_hash,
                );
                Self::deposit_event(RawEvent::NodeContractCanceled(contract_id, node_contract.node_id, contract.twin_id));
            }
            types::ContractData::NameContract(name_contract) => {
                ContractIDByNameRegistration::remove(name_contract.name);
                Self::deposit_event(RawEvent::NameContractCanceled(contract_id));
            }
        };

        Self::_update_contract_state(&mut contract, &types::ContractState::Deleted)?;

        Ok(())
    }

    pub fn _compute_reports(
        source: T::AccountId,
        reports: Vec<types::Consumption>,
    ) -> DispatchResult {
        ensure!(
            pallet_tfgrid::TwinIdByAccountID::<T>::contains_key(&source),
            Error::<T>::TwinNotExists
        );
        let twin_id = pallet_tfgrid::TwinIdByAccountID::<T>::get(&source);
        ensure!(
            pallet_tfgrid::NodeIdByTwinID::contains_key(twin_id),
            Error::<T>::NodeNotExists
        );

        // fetch the node from the source account (signee)
        let node_id = pallet_tfgrid::NodeIdByTwinID::get(&twin_id);
        let node = pallet_tfgrid::Nodes::get(node_id);

        ensure!(
            pallet_tfgrid::Farms::contains_key(&node.farm_id),
            Error::<T>::FarmNotExists
        );
        let farm = pallet_tfgrid::Farms::get(node.farm_id);

        ensure!(
            pallet_tfgrid::PricingPolicies::<T>::contains_key(farm.pricing_policy_id),
            Error::<T>::PricingPolicyNotExists
        );
        let pricing_policy = pallet_tfgrid::PricingPolicies::<T>::get(farm.pricing_policy_id);

        // validation
        for report in &reports {
            if !Contracts::contains_key(report.contract_id) {
                continue;
            }
            let contract = Contracts::get(report.contract_id);
            let node_contract = Self::get_node_contract(&contract)?;
            ensure!(
                node_contract.node_id == node_id,
                Error::<T>::NodeNotAuthorizedToComputeReport
            );
            ensure!(
                ContractBillingInformationByID::contains_key(report.contract_id),
                Error::<T>::ContractNotExists
            );
        }

        for report in reports {
            Self::_calculate_report_cost(&report, &pricing_policy)?;
            Self::deposit_event(RawEvent::ConsumptionReportReceived(report));
        }

        Ok(())
    }

    // Calculates the total cost of a report.
    // Takes in a report, the contract's billing information and the linked farm's pricing policy.
    // Updates the contract's billing information in storage
    pub fn _calculate_report_cost(
        report: &types::Consumption,
        pricing_policy: &pallet_tfgrid_types::PricingPolicy<T::AccountId>,
    ) -> DispatchResult {
        ensure!(
            ContractBillingInformationByID::contains_key(report.contract_id),
            Error::<T>::ContractNotExists
        );
        let mut contract_billing_info = ContractBillingInformationByID::get(report.contract_id);
        if report.timestamp < contract_billing_info.last_updated {
            return Ok(());
        }

        let seconds_elapsed = report.timestamp - contract_billing_info.last_updated;
        debug::info!("seconds elapsed: {:?}", seconds_elapsed);

        let hru = U64F64::from_num(report.hru) / pricing_policy.su.factor();
        let sru = U64F64::from_num(report.sru) / pricing_policy.su.factor();
        let mru = U64F64::from_num(report.mru) / pricing_policy.cu.factor();

        let su_used = hru / 1200 + sru / 300;
        // the pricing policy su cost value is expressed in 1 hours or 3600 seconds.
        // we bill every 3600 seconds but here we need to calculate the cost per second and multiply it by the seconds elapsed since last report.
        let su_cost = (U64F64::from_num(pricing_policy.su.value) / 3600)
            * U64F64::from_num(seconds_elapsed)
            * su_used;
        debug::info!("su cost: {:?}", su_cost);

        let mru_used = mru / 4;
        let cru_used = U64F64::from_num(report.cru) / 2;
        let min = if mru_used < cru_used {
            mru_used
        } else {
            cru_used
        };
        let cu_cost = (U64F64::from_num(pricing_policy.cu.value) / 3600)
            * U64F64::from_num(seconds_elapsed)
            * min;
        debug::info!("cu cost: {:?}", cu_cost);

        let mut used_nru = U64F64::from_num(report.nru) / pricing_policy.nu.factor();
        let nu_cost = if used_nru > contract_billing_info.previous_nu_reported {
            // calculate used nru by subtracting previous reported units minus what is reported now
            // this is because nru is in a counter that increases only
            used_nru -= U64F64::from_num(contract_billing_info.previous_nu_reported);

            // calculate the cost for nru based on the used nru
            used_nru * (U64F64::from_num(pricing_policy.nu.value) / 3600)
        } else {
            U64F64::from_num(0)
        };

        debug::info!("nu cost: {:?}", nu_cost);

        // save total
        let total = su_cost + cu_cost + nu_cost;
        let total = total.ceil().to_num::<u64>();
        debug::info!("total cost: {:?}", total);

        contract_billing_info.previous_nu_reported = used_nru.ceil().to_num::<u64>();
        contract_billing_info.amount_unbilled += total;
        contract_billing_info.last_updated = report.timestamp;

        ContractBillingInformationByID::insert(report.contract_id, &contract_billing_info);

        Ok(())
    }

    pub fn _bill_contracts_at_block(block: T::BlockNumber) -> DispatchResult {
        let current_block_u64: u64 = block.saturated_into::<u64>();
        let contracts = ContractsToBillAt::get(current_block_u64);
        debug::info!("Contracts to check at block: {:?}, {:?}", block, contracts);
        for contract_id in contracts {
            let contract = Contracts::get(contract_id);
            if contract.state != types::ContractState::Created {
                continue;
            }
            Self::_bill_contract(contract)?;
        }
        Ok(())
    }

    // Bills a contract based on:
    // Saved amount unbilled on the contract, this is incremented by the node sending capacity reports
    // We calculate total IP cost for the amount between the last billed time and now and add this to the amount due
    // If the user runs out of balance, we decomission the contract and therefor will be removed, ips will be freed as well
    fn _bill_contract(mut contract: types::Contract) -> DispatchResult {
        // Reinsert contract to be billed for the next frequency
        Self::_reinsert_contract_to_bill(contract.contract_id);
        match contract.contract_type {
            types::ContractData::NodeContract(_) => Self::_bill_node_contract(&mut contract)?,
            types::ContractData::NameContract(_) => Self::_bill_name_contract(&mut contract)?,
        };

        Ok(())
    }

    fn _bill_node_contract(contract: &mut types::Contract) -> DispatchResult {
        let mut node_contract = Self::get_node_contract(contract)?;

        let node = pallet_tfgrid::Nodes::get(node_contract.node_id);
        ensure!(
            pallet_tfgrid::Farms::contains_key(&node.farm_id),
            Error::<T>::FarmNotExists
        );

        let farm = pallet_tfgrid::Farms::get(node.farm_id);
        ensure!(
            pallet_tfgrid::PricingPolicies::<T>::contains_key(farm.pricing_policy_id),
            Error::<T>::PricingPolicyNotExists
        );

        let pricing_policy = pallet_tfgrid::PricingPolicies::<T>::get(farm.pricing_policy_id);

        // bill user for 1 hour ip usage (60 blocks * 60 seconds)
        let total_ip_cost = node_contract.public_ips * pricing_policy.ipu.value;

        let mut contract_billing_info = ContractBillingInformationByID::get(contract.contract_id);
        let total_cost = total_ip_cost as u64 + contract_billing_info.amount_unbilled;

        // If cost is 0, reinsert to be billed at next interval
        if total_cost == 0 {
            // Self::_reinsert_contract_to_bill(contract.contract_id)?;
            return Ok(());
        }
        let tft_price = U64F64::from_num(pallet_tft_price::AverageTftPrice::get());
        if tft_price <= U64F64::from_num(0) {
            debug::info!("TFT price is zero");
            return Err(DispatchError::from(Error::<T>::TFTPriceValueError));
        }
        let total_cost_tft = U64F64::from_num(total_cost) / tft_price;
        let total_cost_tft_64 = U64F64::to_num(total_cost_tft);
        let twin = pallet_tfgrid::Twins::<T>::get(contract.twin_id);
        let balance: BalanceOf<T> = <T as Config>::Currency::free_balance(&twin.account_id);

        // Calculate the amount due and discount received based on the total_cost amount due
        let (amount_due, discount_received) =
            Self::_calculate_discount(total_cost_tft_64, balance, farm.certification_type);
        // Convert amount due to u128
        let amount_due_as_u128: u128 = amount_due.saturated_into::<u128>();
        // Get current TFT price

        // if the total amount due exceeds the twin's balance, decomission contract
        // but first drain the account with the amount equal to the balance of that twin
        let mut amount_due: BalanceOf<T> = BalanceOf::<T>::saturated_from(amount_due_as_u128);

        let mut decomission = false;
        if amount_due >= balance {
            amount_due = balance;
            decomission = true;
        }

        // Distribute cultivation rewards
        Self::_distribute_cultivation_rewards(
            &contract,
            &node,
            &farm,
            &pricing_policy,
            amount_due,
        )?;

        let contract_bill = types::ContractBill {
            contract_id: contract.contract_id,
            timestamp: <timestamp::Module<T>>::get().saturated_into::<u64>() / 1000,
            discount_level: discount_received.clone(),
            amount_billed: amount_due_as_u128,
        };
        Self::deposit_event(RawEvent::ContractBilled(contract_bill));

        // If total balance exceeds the twin's balance, we can decomission contract
        if decomission {
            if node_contract.public_ips > 0 {
                Self::_free_ip(contract.contract_id, &mut node_contract)?;
            }
            Self::_update_contract_state(contract, &types::ContractState::OutOfFunds)?;
            return Ok(());
        }

        // set the amount unbilled back to 0
        contract_billing_info.amount_unbilled = 0;
        ContractBillingInformationByID::insert(contract.contract_id, &contract_billing_info);

        Ok(())
    }

    fn _bill_name_contract(contract: &mut types::Contract) -> DispatchResult {
        // TODO: CHANGE THIS!!!!
        // we cannot now which pricing policy to fetch because a name contract is not linked to a node/farm
        let pricing_policy = pallet_tfgrid::PricingPolicies::<T>::get(1);
        let total_name_cost = U64F64::from_num(pricing_policy.unique_name.value);
        // get the contract's twin free balance
        let twin = pallet_tfgrid::Twins::<T>::get(contract.twin_id);
        let balance: BalanceOf<T> = <T as Config>::Currency::free_balance(&twin.account_id);
        debug::info!("free balance: {:?}", balance);

        //Get TFT price
        let tft_price = U64F64::from_num(pallet_tft_price::AverageTftPrice::get());
        if tft_price <= U64F64::from_num(0) {
            debug::info!("TFT price is zero");
            return Err(DispatchError::from(Error::<T>::TFTPriceValueError));
        }
        //Convert amount due to TFT
        let amount_due_tft = total_name_cost / tft_price;
        // // Calculate the amount due and discount received based on the total_cost amount due
        // let (mut amount_due, discount_received) = Self::_calculate_discount(total_name_cost, balance, farm.certification_type);
        // // Convert amount due to u128
        let mut amount_due_tft_as_u128: u128 = U64F64::to_num(amount_due_tft);
        let balance_as_u128: u128 = balance.saturated_into::<u128>();

        // if the total amount due exceeds the twin's balance, decomission contract
        // but first drain the account with the amount equal to the balance of that twin
        let mut decomission = false;
        if amount_due_tft_as_u128 >= balance_as_u128 {
            amount_due_tft_as_u128 = balance_as_u128;
            decomission = true;
        }

        let contract_bill = types::ContractBill {
            contract_id: contract.contract_id,
            timestamp: <timestamp::Module<T>>::get().saturated_into::<u64>() / 1000,
            // TODO: maybe change this
            discount_level: types::DiscountLevel::None,
            amount_billed: amount_due_tft_as_u128,
        };
        Self::deposit_event(RawEvent::ContractBilled(contract_bill));

        // If total balance exceeds the twin's balance, we can decomission contract
        if decomission {
            Self::_update_contract_state(contract, &types::ContractState::OutOfFunds)?;
            return Ok(());
        }

        Ok(())
    }

    // Following: https://github.com/threefoldfoundation/info_threefold/blob/development/wiki/farming/farming3/cultivation_flow.md
    fn _distribute_cultivation_rewards(
        contract: &types::Contract,
        node: &pallet_tfgrid_types::Node,
        farm: &pallet_tfgrid_types::Farm,
        pricing_policy: &pallet_tfgrid_types::PricingPolicy<T::AccountId>,
        amount: BalanceOf<T>,
    ) -> DispatchResult {
        // fetch farmer twin
        let farmer_twin = pallet_tfgrid::Twins::<T>::get(farm.twin_id);
        // fetch source twin
        let twin = pallet_tfgrid::Twins::<T>::get(contract.twin_id);

        // parse amount
        let amount = U64F64::from_num(amount.saturated_into::<u128>());

        let foundation_share = amount * U64F64::from_num(0.3);
        let mut certified_sales_share = amount * U64F64::from_num(0.6);

        // by default, 10% of the cultivation rewards go to the farmer
        let mut farmer_share = amount * U64F64::from_num(0.1);
        // if the farmer is deploying workloads on his own farm, return to him 70% of the total amount
        if node.farm_id == farm.id {
            farmer_share = amount * U64F64::from_num(0.7);
            certified_sales_share = U64F64::from_num(0);
        }

        // Tranfer to foundation account
        let foundation_share_balance =
            BalanceOf::<T>::saturated_from(foundation_share.ceil().to_num::<u128>());
        debug::info!(
            "Transfering: {:?} from contract twin {:?} to foundation account {:?}",
            &foundation_share_balance,
            &twin.account_id,
            &pricing_policy.foundation_account
        );
        <T as Config>::Currency::transfer(
            &twin.account_id,
            &pricing_policy.foundation_account,
            foundation_share_balance,
            KeepAlive,
        )
        .map_err(|_| DispatchError::Other("Can't make foundation share transfer"))?;

        // Transfer to farmer account
        let farmers_share_balance =
            BalanceOf::<T>::saturated_from(farmer_share.ceil().to_num::<u128>());
        debug::info!(
            "Transfering: {:?} from contract twin {:?} to foundation account {:?}",
            &farmers_share_balance,
            &twin.account_id,
            &farmer_twin.account_id
        );
        <T as Config>::Currency::transfer(
            &twin.account_id,
            &farmer_twin.account_id,
            farmers_share_balance,
            KeepAlive,
        )
        .map_err(|_| DispatchError::Other("Can't make farmer share transfer"))?;

        // Transfer to sales account if applied
        let certified_sales_share_u128 = certified_sales_share.ceil().to_num::<u128>();
        if certified_sales_share > 0 {
            let sales_share_balance = BalanceOf::<T>::saturated_from(certified_sales_share_u128);
            debug::info!(
                "Transfering: {:?} from contract twin {:?} to foundation account {:?}",
                &sales_share_balance,
                &twin.account_id,
                &pricing_policy.certified_sales_account
            );
            <T as Config>::Currency::transfer(
                &twin.account_id,
                &pricing_policy.certified_sales_account,
                sales_share_balance,
                KeepAlive,
            )
            .map_err(|_| DispatchError::Other("Can't make sales share transfer"))?;
        }

        Ok(())
    }

    // Calculates the discount that will be applied to the billing of the contract
    // Returns an amount due as balance object and a static string indicating which kind of discount it received
    // (default, bronze, silver, gold or none)
    fn _calculate_discount(
        amount_due: u64,
        balance: BalanceOf<T>,
        certification_type: pallet_tfgrid_types::CertificationType,
    ) -> (BalanceOf<T>, types::DiscountLevel) {
        let balance_as_u128: u128 = balance.saturated_into::<u128>();

        // calculate amount due on a monthly basis
        // we bill every one hour so we can infer the amount due monthly (30 days ish)
        let amount_due_monthly = amount_due * 24 * 30;

        // see how many months a user can pay for this deployment given his balance
        let discount_level =
            U64F64::from_num(balance_as_u128) / U64F64::from_num(amount_due_monthly);

        // predefined discount levels
        // https://wiki.threefold.io/#/threefold__grid_pricing
        let discount_received = match discount_level.floor().to_num::<u64>() {
            d if d >= 3 && d < 6 => types::DiscountLevel::Default,
            d if d >= 6 && d < 12 => types::DiscountLevel::Bronze,
            d if d >= 12 && d < 36 => types::DiscountLevel::Silver,
            d if d >= 36 => types::DiscountLevel::Gold,
            _ => types::DiscountLevel::None,
        };

        // calculate the new amount due given the discount
        let mut amount_due = U64F64::from_num(amount_due) * discount_received.price_multiplier();

        // Certified capacity costs 25% more
        if certification_type == pallet_tfgrid_types::CertificationType::Certified {
            amount_due = amount_due * U64F64::from_num(1.25);
        }

        // convert to balance object
        let amount_due: BalanceOf<T> =
            BalanceOf::<T>::saturated_from(amount_due.ceil().to_num::<u64>());

        (amount_due, discount_received)
    }

    // Reinserts a contract by id at the next interval we need to bill the contract
    pub fn _reinsert_contract_to_bill(contract_id: u64) {
        let now = <frame_system::Module<T>>::block_number().saturated_into::<u64>();
        // Save the contract to be billed in now + BILLING_FREQUENCY_IN_BLOCKS
        let future_block = now + BILLING_FREQUENCY_IN_BLOCKS;
        let mut contracts = ContractsToBillAt::get(future_block);
        contracts.push(contract_id);
        ContractsToBillAt::insert(future_block, &contracts);
        debug::info!(
            "Insert contracts: {:?}, to be billed at block {:?}",
            contracts,
            future_block
        );
    }

    // Helper function that updates the contract state and manages storage accordingly
    pub fn _update_contract_state(
        contract: &mut types::Contract,
        state: &types::ContractState,
    ) -> DispatchResult {
        match &contract.contract_type {
            types::ContractData::NodeContract(node_contract) => {
                // Remove contract from double map first
                let mut contracts = NodeContracts::get(&node_contract.node_id, &contract.state);

                match contracts
                    .iter()
                    .position(|ct| ct.contract_id == contract.contract_id)
                {
                    Some(index) => {
                        // remove contract with state from double map first
                        contracts.remove(index);
                        NodeContracts::insert(&node_contract.node_id, &contract.state, &contracts);

                        // assign new state
                        contract.state = state.clone();
                        contracts.insert(index, contract.clone());
                    }
                    None => {
                        contracts.push(contract.clone());
                    }
                };

                // insert contract with new state into double map
                NodeContracts::insert(&node_contract.node_id, &contract.state, &contracts);
            }
            _ => (),
        }
        contract.state = state.clone();
        // update Contracts storage as well
        Contracts::insert(&contract.contract_id.clone(), contract);

        Ok(())
    }

    pub fn _reserve_ip(
        contract_id: u64,
        node_contract: &mut types::NodeContract,
    ) -> DispatchResult {
        if node_contract.public_ips == 0 {
            return Ok(());
        }
        let node = pallet_tfgrid::Nodes::get(node_contract.node_id);

        ensure!(
            pallet_tfgrid::Farms::contains_key(&node.farm_id),
            Error::<T>::FarmNotExists
        );
        let mut farm = pallet_tfgrid::Farms::get(node.farm_id);

        debug::info!(
            "Number of farm ips {:?}, number of ips to reserve: {:?}",
            farm.public_ips.len(),
            node_contract.public_ips as usize
        );
        ensure!(
            farm.public_ips.len() >= node_contract.public_ips as usize,
            Error::<T>::FarmHasNotEnoughPublicIPs
        );

        let mut ips = Vec::new();
        for i in 0..farm.public_ips.len() {
            let mut ip = farm.public_ips[i].clone();

            if ips.len() == node_contract.public_ips as usize {
                break;
            }

            // if an ip has contract id 0 it means it's not reserved
            // reserve it now
            if ip.contract_id == 0 {
                ip.contract_id = contract_id;
                farm.public_ips[i] = ip.clone();
                ips.push(ip);
            }
        }

        // Safeguard check if we actually have the amount of ips we wanted to reserve
        ensure!(
            ips.len() == node_contract.public_ips as usize,
            Error::<T>::FarmHasNotEnoughPublicIPsFree
        );

        // Update the farm with the reserved ips
        pallet_tfgrid::Farms::insert(farm.id, farm);

        node_contract.public_ips_list = ips;

        Ok(())
    }

    pub fn _free_ip(contract_id: u64, node_contract: &mut types::NodeContract) -> DispatchResult {
        let node = pallet_tfgrid::Nodes::get(node_contract.node_id);

        ensure!(
            pallet_tfgrid::Farms::contains_key(&node.farm_id),
            Error::<T>::FarmNotExists
        );
        let mut farm = pallet_tfgrid::Farms::get(node.farm_id);

        let mut ips_freed = Vec::new();
        for i in 0..farm.public_ips.len() {
            let mut ip = farm.public_ips[i].clone();

            // if an ip has contract id 0 it means it's not reserved
            // reserve it now
            if ip.contract_id == contract_id {
                ip.contract_id = 0;
                farm.public_ips[i] = ip.clone();
                ips_freed.push(ip.ip);
            }
        }

        pallet_tfgrid::Farms::insert(farm.id, farm);

        // Emit an event containing the IP's freed for this contract
        Self::deposit_event(RawEvent::IPsFreed(contract_id, ips_freed));

        Ok(())
    }

    // Registers a DNS name for a Twin
    // Ensures uniqueness and also checks if it's a valid DNS name
    pub fn _create_name_contract(source: T::AccountId, name: Vec<u8>) -> DispatchResult {
        ensure!(
            pallet_tfgrid::TwinIdByAccountID::<T>::contains_key(&source),
            Error::<T>::TwinNotExists
        );
        let twin_id = pallet_tfgrid::TwinIdByAccountID::<T>::get(&source);

        // Validate name uniqueness
        ensure!(
            !ContractIDByNameRegistration::contains_key(&name),
            Error::<T>::NameExists
        );

        for character in &name {
            match character {
                c if *c == 45 => (),
                c if *c >= 48 && *c <= 57 => (),
                c if *c >= 65 && *c <= 122 => (),
                _ => return Err(DispatchError::from(Error::<T>::NameNotValid)),
            }
        }
        let name_contract = types::NameContract { name: name.clone() };
        let contract =
            Self::_create_contract(twin_id, types::ContractData::NameContract(name_contract))?;

        ContractIDByNameRegistration::insert(name, &contract.contract_id);

        Self::deposit_event(RawEvent::ContractCreated(contract));

        Ok(())
    }

    fn get_node_contract(contract: &types::Contract) -> Result<types::NodeContract, DispatchError> {
        match contract.contract_type.clone() {
            types::ContractData::NodeContract(c) => Ok(c),
            _ => return Err(DispatchError::from(Error::<T>::InvalidContractType)),
        }
    }

    // fn get_name_contract(contract: &types::Contract) -> Result<types::NameContract, DispatchError> {
    // 	match contract.contract_type.clone() {
    // 		types::ContractData::NameContract(c) => Ok(c),
    // 		_ => return Err(DispatchError::from(Error::<T>::InvalidContractType)),
    // 	}
    // }
}
