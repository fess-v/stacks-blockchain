// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

pub mod contexts;
//mod maps;
pub mod natives;

use crate::vm::costs::{
    analysis_typecheck_cost, cost_functions, runtime_cost, ClarityCostFunctionReference,
    CostErrors, CostOverflowingMath, CostTracker, ExecutionCost, LimitedCostTracker,
};
use crate::vm::functions::define::DefineFunctionsParsed;
use crate::vm::functions::NativeFunctions;
use crate::vm::representations::SymbolicExpressionType::{
    Atom, AtomValue, Field, List, LiteralValue, TraitReference,
};
use crate::vm::representations::{depth_traverse, ClarityName, SymbolicExpression};
use crate::vm::types::signatures::{CallableSubtype, FunctionSignature, BUFF_20};
use crate::vm::types::{
    parse_name_type_pairs, CallableData, FixedFunction, FunctionArg, FunctionType, ListData,
    ListTypeData, OptionalData, PrincipalData, QualifiedContractIdentifier, ResponseData,
    SequenceData, SequenceSubtype, StringSubtype, TraitIdentifier, TupleData, TupleTypeSignature,
    TypeSignature, Value, MAX_TYPE_DEPTH,
};
use crate::vm::variables::NativeVariables;
use std::collections::{BTreeMap, HashMap};
use std::convert::TryInto;

use crate::vm::ClarityVersion;

pub use super::types::{AnalysisPass, ContractAnalysis};
use super::AnalysisDatabase;

use self::contexts::{ContractContext, TypeMap, TypingContext};

pub use self::natives::{SimpleNativeFunction, TypedNativeFunction};

pub use super::errors::{
    check_argument_count, check_arguments_at_least, check_arguments_at_most, CheckError,
    CheckErrors, CheckResult,
};
use crate::vm::contexts::Environment;
use crate::vm::costs::cost_functions::ClarityCostFunction;

#[cfg(test)]
pub mod tests;

/*

Type-checking in our language is achieved through a single-direction inference.
This leads to efficient type-checking. This form of type-checking is only possible
due to the rules of our language. In particular, functions define their input types,
and any given intermediate in the language has a strict type as well, meaning something
of the form:

(if x
   true
   -1)

Is illegally typed in our language.

*/

pub struct TypeChecker<'a, 'b> {
    pub type_map: TypeMap,
    contract_context: ContractContext,
    function_return_tracker: Option<Option<TypeSignature>>,
    db: &'a mut AnalysisDatabase<'b>,
    pub cost_track: LimitedCostTracker,
    clarity_version: ClarityVersion,
}

impl CostTracker for TypeChecker<'_, '_> {
    fn compute_cost(
        &mut self,
        cost_function: ClarityCostFunction,
        input: &[u64],
    ) -> Result<ExecutionCost, CostErrors> {
        self.cost_track.compute_cost(cost_function, input)
    }

    fn add_cost(&mut self, cost: ExecutionCost) -> std::result::Result<(), CostErrors> {
        self.cost_track.add_cost(cost)
    }
    fn add_memory(&mut self, memory: u64) -> std::result::Result<(), CostErrors> {
        self.cost_track.add_memory(memory)
    }
    fn drop_memory(&mut self, memory: u64) {
        self.cost_track.drop_memory(memory)
    }
    fn reset_memory(&mut self) {
        self.cost_track.reset_memory()
    }
    fn short_circuit_contract_call(
        &mut self,
        contract: &QualifiedContractIdentifier,
        function: &ClarityName,
        input: &[u64],
    ) -> std::result::Result<bool, CostErrors> {
        self.cost_track
            .short_circuit_contract_call(contract, function, input)
    }
}

impl AnalysisPass for TypeChecker<'_, '_> {
    fn run_pass(
        contract_analysis: &mut ContractAnalysis,
        analysis_db: &mut AnalysisDatabase,
    ) -> CheckResult<()> {
        let cost_track = contract_analysis.take_contract_cost_tracker();
        let mut command = TypeChecker::new(
            analysis_db,
            cost_track,
            &contract_analysis.contract_identifier,
            &contract_analysis.clarity_version,
        );
        // run the analysis, and replace the cost tracker whether or not the
        //   analysis succeeded.
        match command.run(contract_analysis) {
            Ok(_) => {
                let cost_track = command.into_contract_analysis(contract_analysis);
                contract_analysis.replace_contract_cost_tracker(cost_track);
                Ok(())
            }
            err => {
                let TypeChecker { cost_track, .. } = command;
                contract_analysis.replace_contract_cost_tracker(cost_track);
                err
            }
        }
    }
}

pub type TypeResult = CheckResult<TypeSignature>;

impl FunctionType {
    pub fn check_args<T: CostTracker>(
        &self,
        accounting: &mut T,
        args: &[TypeSignature],
        clarity_version: ClarityVersion,
    ) -> CheckResult<TypeSignature> {
        match self {
            FunctionType::Variadic(expected_type, return_type) => {
                check_arguments_at_least(1, args)?;
                for found_type in args.iter() {
                    analysis_typecheck_cost(accounting, expected_type, found_type)?;
                    if !expected_type.admits_type(found_type) {
                        return Err(CheckErrors::TypeError(
                            expected_type.clone(),
                            found_type.clone(),
                        )
                        .into());
                    }
                }
                Ok(return_type.clone())
            }
            FunctionType::Fixed(FixedFunction {
                args: arg_types,
                returns,
            }) => {
                check_argument_count(arg_types.len(), args)?;
                for (expected_type, found_type) in arg_types.iter().map(|x| &x.signature).zip(args)
                {
                    analysis_typecheck_cost(accounting, expected_type, found_type)?;
                    if !expected_type.admits_type(found_type) {
                        return Err(CheckErrors::TypeError(
                            expected_type.clone(),
                            found_type.clone(),
                        )
                        .into());
                    }
                }
                Ok(returns.clone())
            }
            FunctionType::UnionArgs(arg_types, return_type) => {
                check_argument_count(1, args)?;
                let found_type = &args[0];
                for expected_type in arg_types.iter() {
                    analysis_typecheck_cost(accounting, expected_type, found_type)?;
                    if expected_type.admits_type(found_type) {
                        return Ok(return_type.clone());
                    }
                }
                Err(CheckErrors::UnionTypeError(arg_types.clone(), found_type.clone()).into())
            }
            FunctionType::ArithmeticVariadic
            | FunctionType::ArithmeticBinary
            | FunctionType::ArithmeticUnary => {
                if self == &FunctionType::ArithmeticUnary {
                    check_argument_count(1, args)?;
                }
                if self == &FunctionType::ArithmeticBinary {
                    check_argument_count(2, args)?;
                }
                let (first, rest) = args
                    .split_first()
                    .ok_or(CheckErrors::RequiresAtLeastArguments(1, args.len()))?;
                analysis_typecheck_cost(accounting, &TypeSignature::IntType, first)?;
                let return_type = match first {
                    TypeSignature::IntType => Ok(TypeSignature::IntType),
                    TypeSignature::UIntType => Ok(TypeSignature::UIntType),
                    _ => Err(CheckErrors::UnionTypeError(
                        vec![TypeSignature::IntType, TypeSignature::UIntType],
                        first.clone(),
                    )),
                }?;
                for found_type in rest.iter() {
                    analysis_typecheck_cost(accounting, &TypeSignature::IntType, found_type)?;
                    if found_type != &return_type {
                        return Err(CheckErrors::TypeError(return_type, found_type.clone()).into());
                    }
                }
                Ok(return_type)
            }
            FunctionType::ArithmeticComparison => {
                check_argument_count(2, args)?;
                let (first, second) = (&args[0], &args[1]);
                analysis_typecheck_cost(accounting, &TypeSignature::IntType, first)?;
                analysis_typecheck_cost(accounting, &TypeSignature::IntType, second)?;

                // Note: Clarity2 expanded the comparable types to include ASCII, UTF8 and Buffer.
                // Int and UInt have been present since Clarity1.
                let is_clarity2: bool = clarity_version >= ClarityVersion::Clarity2;
                // Step 1: Check the first argument on its own, to see that the first argument
                // has a supported type according to this ClarityVersion.
                let first_type_supported = match first {
                    TypeSignature::IntType => true,
                    TypeSignature::UIntType => true,
                    TypeSignature::SequenceType(SequenceSubtype::StringType(
                        StringSubtype::ASCII(_),
                    )) => is_clarity2,
                    TypeSignature::SequenceType(SequenceSubtype::StringType(
                        StringSubtype::UTF8(_),
                    )) => is_clarity2,
                    TypeSignature::SequenceType(SequenceSubtype::BufferType(_)) => is_clarity2,
                    _ => false,
                };

                if !first_type_supported {
                    return Err(CheckErrors::UnionTypeError(
                        vec![
                            TypeSignature::IntType,
                            TypeSignature::UIntType,
                            TypeSignature::max_string_ascii(),
                            TypeSignature::max_string_utf8(),
                            TypeSignature::max_buffer(),
                        ],
                        first.clone(),
                    )
                    .into());
                }

                // Step 2: Assuming the first argument has a supported type, now check that
                // both of the types are matching.
                let pair_of_types_matches = match (first, second) {
                    (TypeSignature::IntType, TypeSignature::IntType) => true,
                    (TypeSignature::UIntType, TypeSignature::UIntType) => true,
                    (
                        TypeSignature::SequenceType(SequenceSubtype::StringType(
                            StringSubtype::ASCII(_),
                        )),
                        TypeSignature::SequenceType(SequenceSubtype::StringType(
                            StringSubtype::ASCII(_),
                        )),
                    ) => true,
                    (
                        TypeSignature::SequenceType(SequenceSubtype::StringType(
                            StringSubtype::UTF8(_),
                        )),
                        TypeSignature::SequenceType(SequenceSubtype::StringType(
                            StringSubtype::UTF8(_),
                        )),
                    ) => true,
                    (
                        TypeSignature::SequenceType(SequenceSubtype::BufferType(_)),
                        TypeSignature::SequenceType(SequenceSubtype::BufferType(_)),
                    ) => true,
                    (_, _) => false,
                };

                if !pair_of_types_matches {
                    return Err(CheckErrors::TypeError(first.clone(), second.clone()).into());
                }

                Ok(TypeSignature::BoolType)
            }
        }
    }

    /// Returns the type of `value`, after converting any contract principal
    /// types to callable types. In an initial transaction, arguments are typed
    /// as contract principals, but they must be principal literals, so they
    /// may be used to call into a contract.
    pub fn principal_to_callable_type(&self, value: &Value, depth: u8) -> TypeResult {
        if depth > MAX_TYPE_DEPTH {
            return Err(CheckErrors::TypeSignatureTooDeep.into());
        }

        Ok(match value {
            Value::Principal(PrincipalData::Contract(contract_identifier)) => {
                TypeSignature::CallableType(CallableSubtype::Principal(contract_identifier.clone()))
            }
            Value::Optional(OptionalData {
                data: Some(inner_value),
            }) => {
                TypeSignature::new_option(self.principal_to_callable_type(inner_value, depth + 1)?)?
            }
            Value::Response(ResponseData { committed, data }) => {
                let (ok_type, err_type) = if *committed {
                    (
                        self.principal_to_callable_type(data, depth + 1)?,
                        TypeSignature::NoType,
                    )
                } else {
                    (
                        TypeSignature::NoType,
                        self.principal_to_callable_type(data, depth + 1)?,
                    )
                };
                TypeSignature::new_response(ok_type, err_type)?
            }
            Value::Sequence(SequenceData::List(ListData {
                data,
                type_signature: _,
            })) => {
                let inner_type = match data.first() {
                    Some(inner_val) => self.principal_to_callable_type(inner_val, depth + 1)?,
                    None => TypeSignature::NoType,
                };
                TypeSignature::SequenceType(SequenceSubtype::ListType(ListTypeData::new_list(
                    inner_type,
                    data.len() as u32,
                )?))
            }
            Value::Tuple(TupleData {
                type_signature: _,
                data_map,
            }) => {
                let mut type_map = BTreeMap::new();
                for (name, field_value) in data_map {
                    type_map.insert(
                        name.clone(),
                        self.principal_to_callable_type(field_value, depth + 1)?,
                    );
                }
                TypeSignature::TupleType(TupleTypeSignature::try_from(type_map)?)
            }
            _ => TypeSignature::type_of(value),
        })
    }

    pub fn check_args_by_allowing_trait_cast(
        &self,
        db: &mut AnalysisDatabase,
        clarity_version: ClarityVersion,
        func_args: &[Value],
    ) -> CheckResult<TypeSignature> {
        let (expected_args, returns) = match self {
            FunctionType::Fixed(FixedFunction { args, returns }) => (args, returns),
            _ => panic!("Unexpected function type"),
        };
        check_argument_count(expected_args.len(), func_args)?;

        let mut arg_types = Vec::new();
        for arg in func_args {
            arg_types.push(self.principal_to_callable_type(arg, 1)?);
        }

        for (expected_arg, arg_type) in expected_args.iter().zip(arg_types.iter()).into_iter() {
            inner_type_check_type(
                db,
                None,
                arg_type,
                &expected_arg.signature,
                clarity_version,
                1,
                &mut LimitedCostTracker::new_free(),
            )?;
        }
        Ok(returns.clone())
    }
}

pub fn trait_check_trait_compliance<T: CostTracker>(
    db: &mut AnalysisDatabase,
    contract_context: Option<&ContractContext>,
    actual_trait_identifier: &TraitIdentifier,
    actual_trait: &BTreeMap<ClarityName, FunctionSignature>,
    expected_trait_identifier: &TraitIdentifier,
    expected_trait: &BTreeMap<ClarityName, FunctionSignature>,
    clarity_version: ClarityVersion,
    tracker: &mut T,
) -> CheckResult<()> {
    // Shortcut for the simple case when the two traits are the same.
    if actual_trait_identifier == expected_trait_identifier {
        return Ok(());
    }

    for (func_name, expected_sig) in expected_trait.iter() {
        if let Some(func) = actual_trait.get(func_name) {
            let args_iter = expected_sig.args.iter().zip(func.args.iter());
            for (expected_type, actual_type) in args_iter {
                if inner_type_check_type(
                    db,
                    contract_context,
                    actual_type,
                    expected_type,
                    clarity_version,
                    1,
                    tracker,
                )
                .is_err()
                {
                    return Err(CheckErrors::IncompatibleTrait(
                        expected_trait_identifier.clone(),
                        actual_trait_identifier.clone(),
                    )
                    .into());
                }
            }
            if inner_type_check_type(
                db,
                contract_context,
                &func.returns,
                &expected_sig.returns,
                clarity_version,
                1,
                tracker,
            )
            .is_err()
            {
                return Err(CheckErrors::IncompatibleTrait(
                    expected_trait_identifier.clone(),
                    actual_trait_identifier.clone(),
                )
                .into());
            }
        } else {
            return Err(CheckErrors::IncompatibleTrait(
                expected_trait_identifier.clone(),
                actual_trait_identifier.clone(),
            )
            .into());
        }
    }
    Ok(())
}

fn inner_type_check_type<T: CostTracker>(
    db: &mut AnalysisDatabase,
    contract_context: Option<&ContractContext>,
    actual_type: &TypeSignature,
    expected_type: &TypeSignature,
    clarity_version: ClarityVersion,
    depth: u8,
    tracker: &mut T,
) -> TypeResult {
    if depth > MAX_TYPE_DEPTH {
        return Err(CheckErrors::TypeSignatureTooDeep.into());
    }

    // Recurse into values to check embedded traits properly
    match (actual_type, expected_type) {
        (
            TypeSignature::OptionalType(atom_inner_type),
            TypeSignature::OptionalType(expected_inner_type),
        ) => {
            inner_type_check_type(
                db,
                contract_context,
                atom_inner_type,
                expected_inner_type,
                clarity_version,
                depth + 1,
                tracker,
            )?;
        }
        (
            TypeSignature::ResponseType(atom_inner_types),
            TypeSignature::ResponseType(expected_inner_types),
        ) => {
            inner_type_check_type(
                db,
                contract_context,
                &atom_inner_types.0,
                &expected_inner_types.0,
                clarity_version,
                depth + 1,
                tracker,
            )?;
            inner_type_check_type(
                db,
                contract_context,
                &atom_inner_types.1,
                &expected_inner_types.1,
                clarity_version,
                depth + 1,
                tracker,
            )?;
        }
        (
            TypeSignature::SequenceType(SequenceSubtype::ListType(atom_list_type)),
            TypeSignature::SequenceType(SequenceSubtype::ListType(expected_list_type)),
        ) => {
            inner_type_check_type(
                db,
                contract_context,
                atom_list_type.get_list_item_type(),
                expected_list_type.get_list_item_type(),
                clarity_version,
                depth + 1,
                tracker,
            )?;
        }
        (
            TypeSignature::TupleType(atom_tuple_type),
            TypeSignature::TupleType(expected_tuple_type),
        ) => {
            if expected_tuple_type.get_type_map().len() != atom_tuple_type.get_type_map().len() {
                return Err(
                    CheckErrors::TypeError(expected_type.clone(), actual_type.clone()).into(),
                );
            }

            for (name, expected_field_type) in expected_tuple_type.get_type_map() {
                match atom_tuple_type.field_type(name) {
                    Some(atom_field_type) => {
                        inner_type_check_type(
                            db,
                            contract_context,
                            atom_field_type,
                            expected_field_type,
                            clarity_version,
                            depth + 1,
                            tracker,
                        )?;
                    }
                    None => {
                        return Err(CheckErrors::TypeError(
                            expected_type.clone(),
                            actual_type.clone(),
                        )
                        .into())
                    }
                }
            }
        }
        (
            TypeSignature::CallableType(CallableSubtype::Trait(atom_trait_id)),
            TypeSignature::CallableType(CallableSubtype::Trait(expected_trait_id)),
        ) => {
            if atom_trait_id != expected_trait_id {
                let atom_trait = lookup_trait(
                    db,
                    contract_context,
                    &atom_trait_id,
                    clarity_version,
                    tracker,
                )?;
                let expected_trait = lookup_trait(
                    db,
                    contract_context,
                    expected_trait_id,
                    clarity_version,
                    tracker,
                )?;
                trait_check_trait_compliance(
                    db,
                    contract_context,
                    &atom_trait_id,
                    &atom_trait,
                    expected_trait_id,
                    &expected_trait,
                    clarity_version,
                    tracker,
                )?;
            }
        }
        (
            TypeSignature::CallableType(CallableSubtype::Principal(contract_identifier)),
            TypeSignature::CallableType(CallableSubtype::Trait(expected_trait_id)),
        ) => {
            let contract_to_check = db
                .load_contract(&contract_identifier)
                .ok_or(CheckErrors::NoSuchContract(contract_identifier.to_string()))?;
            runtime_cost(ClarityCostFunction::AnalysisFetchContractEntry, tracker, 1)?;
            let expected_trait = lookup_trait(
                db,
                contract_context,
                expected_trait_id,
                clarity_version,
                tracker,
            )?;
            contract_to_check.check_trait_compliance(expected_trait_id, &expected_trait)?;
        }
        (
            TypeSignature::ListUnionType(types),
            TypeSignature::CallableType(CallableSubtype::Trait(_)),
        ) => {
            // Verify that all types in the union implement this trait
            for subtype in types {
                inner_type_check_type(
                    db,
                    contract_context,
                    &TypeSignature::CallableType(subtype.clone()),
                    expected_type,
                    clarity_version,
                    depth + 1,
                    tracker,
                )?;
            }
        }
        (TypeSignature::NoType, _) => (),
        (_, _) => {
            if !expected_type.admits_type(&actual_type) {
                return Err(
                    CheckErrors::TypeError(expected_type.clone(), actual_type.clone()).into(),
                );
            }
        }
    }
    Ok(expected_type.clone())
}

fn lookup_trait<T: CostTracker>(
    db: &mut AnalysisDatabase,
    contract_context: Option<&ContractContext>,
    trait_id: &TraitIdentifier,
    clarity_version: ClarityVersion,
    tracker: &mut T,
) -> CheckResult<BTreeMap<ClarityName, FunctionSignature>> {
    if let Some(contract_context) = contract_context {
        // If the trait is from this contract, then it must be in the context or it doesn't exist.
        if contract_context.is_contract(&trait_id.contract_identifier) {
            return Ok(if clarity_version < ClarityVersion::Clarity2 {
                contract_context.get_trait(&trait_id.name)
            } else {
                contract_context.get_trait_by_id(trait_id)
            }
            .ok_or(CheckErrors::NoSuchTrait(
                trait_id.contract_identifier.to_string(),
                trait_id.name.to_string(),
            ))?
            .clone());
        }
        if clarity_version >= ClarityVersion::Clarity2 {
            if let Some(trait_sig) = contract_context.get_trait_by_id(trait_id) {
                return Ok(trait_sig.clone());
            }
        }
    }

    match db
        .get_defined_trait(&trait_id.contract_identifier, &trait_id.name)?
        .ok_or(
            CheckErrors::NoSuchTrait(
                trait_id.contract_identifier.to_string(),
                trait_id.name.to_string(),
            )
            .into(),
        ) {
        Ok(found) => {
            let type_size = trait_type_size(&found)?;
            runtime_cost(
                ClarityCostFunction::AnalysisUseTraitEntry,
                tracker,
                type_size,
            )?;
            Ok(found)
        }
        Err(e) => {
            runtime_cost(ClarityCostFunction::AnalysisUseTraitEntry, tracker, 1)?;
            Err(e)
        }
    }
}

fn trait_type_size(trait_sig: &BTreeMap<ClarityName, FunctionSignature>) -> CheckResult<u64> {
    let mut total_size = 0;
    for (_func_name, value) in trait_sig.iter() {
        total_size = total_size.cost_overflow_add(value.total_type_size()? as u64)?;
    }
    Ok(total_size)
}

fn type_reserved_variable(variable_name: &str, version: &ClarityVersion) -> Option<TypeSignature> {
    if let Some(variable) = NativeVariables::lookup_by_name_at_version(variable_name, version) {
        use crate::vm::variables::NativeVariables::*;
        let var_type = match variable {
            TxSender => TypeSignature::PrincipalType,
            TxSponsor => TypeSignature::new_option(TypeSignature::PrincipalType).unwrap(),
            ContractCaller => TypeSignature::PrincipalType,
            BlockHeight => TypeSignature::UIntType,
            BurnBlockHeight => TypeSignature::UIntType,
            NativeNone => TypeSignature::new_option(no_type()).unwrap(),
            NativeTrue => TypeSignature::BoolType,
            NativeFalse => TypeSignature::BoolType,
            TotalLiquidMicroSTX => TypeSignature::UIntType,
            Regtest => TypeSignature::BoolType,
            Mainnet => TypeSignature::BoolType,
            ChainId => TypeSignature::UIntType,
        };
        Some(var_type)
    } else {
        None
    }
}

pub fn no_type() -> TypeSignature {
    TypeSignature::NoType
}

impl<'a, 'b> TypeChecker<'a, 'b> {
    fn new(
        db: &'a mut AnalysisDatabase<'b>,
        cost_track: LimitedCostTracker,
        contract_identifier: &QualifiedContractIdentifier,
        clarity_version: &ClarityVersion,
    ) -> TypeChecker<'a, 'b> {
        Self {
            db,
            cost_track,
            contract_context: ContractContext::new(contract_identifier.clone()),
            function_return_tracker: None,
            type_map: TypeMap::new(),
            clarity_version: clarity_version.clone(),
        }
    }

    fn into_contract_analysis(
        self,
        contract_analysis: &mut ContractAnalysis,
    ) -> LimitedCostTracker {
        self.contract_context
            .into_contract_analysis(contract_analysis);
        contract_analysis.type_map = Some(self.type_map);
        self.cost_track
    }

    pub fn track_return_type(&mut self, return_type: TypeSignature) -> CheckResult<()> {
        runtime_cost(
            ClarityCostFunction::AnalysisTypeCheck,
            self,
            return_type.type_size()?,
        )?;

        match self.function_return_tracker {
            Some(ref mut tracker) => {
                let new_type = match tracker.take() {
                    Some(expected_type) => {
                        TypeSignature::least_supertype(&expected_type, &return_type).map_err(
                            |_| CheckErrors::ReturnTypesMustMatch(expected_type, return_type),
                        )?
                    }
                    None => return_type,
                };

                tracker.replace(new_type);
                Ok(())
            }
            None => {
                // not in a defining function, so it's okay if aborts, etc., are trying
                //   to return random things, as it'll just error in any case.
                Ok(())
            }
        }
    }

    pub fn run(&mut self, contract_analysis: &mut ContractAnalysis) -> CheckResult<()> {
        // charge for the eventual storage cost of the analysis --
        //  it is linear in the size of the AST.
        let mut size: u64 = 0;
        for exp in contract_analysis.expressions.iter() {
            depth_traverse(exp, |_x| match size.cost_overflow_add(1) {
                Ok(new_size) => {
                    size = new_size;
                    Ok(())
                }
                Err(e) => Err(e),
            })?;
        }

        runtime_cost(ClarityCostFunction::AnalysisStorage, self, size)?;

        let mut local_context = TypingContext::new();

        for exp in contract_analysis.expressions.iter() {
            let mut result_res = self.try_type_check_define(&exp, &mut local_context);
            if let Err(ref mut error) = result_res {
                if !error.has_expression() {
                    error.set_expression(&exp);
                }
            }
            let result = result_res?;
            if result.is_none() {
                // was _not_ a define statement, so handle like a normal statement.
                self.type_check(&exp, &local_context)?;
            }
        }
        Ok(())
    }

    // Type check an expression, with an expected_type that should _admit_ the expression.
    pub fn type_check_expects(
        &mut self,
        expr: &SymbolicExpression,
        context: &TypingContext,
        expected_type: &TypeSignature,
    ) -> TypeResult {
        // Clarity 2 allows traits embedded in compound types and allows
        // implicit casts between compatible traits, while Clarity 1 does not.
        if self.clarity_version >= ClarityVersion::Clarity2 {
            self.clarity2_type_check_expects(expr, context, expected_type)
                .map_err(|mut e| {
                    if !e.has_expression() {
                        e.set_expression(expr)
                    }
                    e
                })
        } else {
            self.clarity1_type_check_expects(expr, context, expected_type)
        }
    }

    // Type checks an expression, recursively type checking its subexpressions
    pub fn type_check(&mut self, expr: &SymbolicExpression, context: &TypingContext) -> TypeResult {
        runtime_cost(ClarityCostFunction::AnalysisVisit, self, 0)?;

        let mut result = self.inner_type_check(expr, context);

        if let Err(ref mut error) = result {
            if !error.has_expression() {
                error.set_expression(expr);
            }
        }

        result
    }

    fn type_check_consecutive_statements(
        &mut self,
        args: &[SymbolicExpression],
        context: &TypingContext,
    ) -> TypeResult {
        let mut types_returned = self.type_check_all(args, context)?;

        let last_return = types_returned
            .pop()
            .ok_or(CheckError::new(CheckErrors::CheckerImplementationFailure))?;

        for type_return in types_returned.iter() {
            if type_return.is_response_type() {
                return Err(CheckErrors::UncheckedIntermediaryResponses.into());
            }
        }
        Ok(last_return)
    }

    fn type_check_all(
        &mut self,
        args: &[SymbolicExpression],
        context: &TypingContext,
    ) -> CheckResult<Vec<TypeSignature>> {
        let mut result = Vec::new();
        for arg in args.iter() {
            // don't use map here, since type_check has side-effects.
            result.push(self.type_check(arg, context)?)
        }
        Ok(result)
    }

    fn type_check_function_type(
        &mut self,
        func_type: &FunctionType,
        args: &[SymbolicExpression],
        context: &TypingContext,
        clarity_version: ClarityVersion,
    ) -> TypeResult {
        let typed_args = self.type_check_all(args, context)?;
        func_type.check_args(self, &typed_args, clarity_version)
    }

    fn get_function_type(&self, function_name: &str) -> Option<FunctionType> {
        self.contract_context
            .get_function_type(function_name)
            .cloned()
    }

    fn type_check_define_function(
        &mut self,
        signature: &[SymbolicExpression],
        body: &SymbolicExpression,
        context: &TypingContext,
    ) -> CheckResult<(ClarityName, FixedFunction)> {
        let (function_name, args) = signature
            .split_first()
            .ok_or(CheckErrors::RequiresAtLeastArguments(1, 0))?;
        let function_name = function_name
            .match_atom()
            .ok_or(CheckErrors::BadFunctionName)?;
        let mut args = parse_name_type_pairs::<()>(args, &mut ())
            .map_err(|_| CheckErrors::BadSyntaxBinding)?;

        if self.function_return_tracker.is_some() {
            panic!("Interpreter error: Previous function define left dirty typecheck state.");
        }

        let mut function_context = context.extend()?;
        for (arg_name, arg_type) in args.iter() {
            self.contract_context.check_name_used(arg_name)?;

            match arg_type {
                TypeSignature::CallableType(CallableSubtype::Trait(trait_id)) => {
                    function_context.add_trait_reference(&arg_name, &trait_id);
                }
                _ => {
                    function_context
                        .variable_types
                        .insert(arg_name.clone(), arg_type.clone());
                }
            }
        }

        self.function_return_tracker = Some(None);

        let return_result = self.type_check(body, &function_context);

        match return_result {
            Err(e) => {
                self.function_return_tracker = None;
                return Err(e);
            }
            Ok(return_type) => {
                let return_type = {
                    if let Some(Some(ref expected)) = self.function_return_tracker {
                        // check if the computed return type matches the return type
                        //   of any early exits from the call graph (e.g., (expects ...) calls)
                        TypeSignature::least_supertype(expected, &return_type).map_err(|_| {
                            CheckErrors::ReturnTypesMustMatch(expected.clone(), return_type)
                        })?
                    } else {
                        return_type
                    }
                };

                self.function_return_tracker = None;

                let func_args: Vec<FunctionArg> = args
                    .drain(..)
                    .map(|(arg_name, arg_type)| FunctionArg::new(arg_type, arg_name))
                    .collect();

                Ok((
                    function_name.clone(),
                    FixedFunction {
                        args: func_args,
                        returns: return_type,
                    },
                ))
            }
        }
    }

    fn type_check_define_map(
        &mut self,
        map_name: &ClarityName,
        key_type: &SymbolicExpression,
        value_type: &SymbolicExpression,
    ) -> CheckResult<(ClarityName, (TypeSignature, TypeSignature))> {
        self.type_map.set_type(key_type, no_type())?;
        self.type_map.set_type(value_type, no_type())?;
        // should we set the type of the subexpressions of the signature to no-type as well?

        let key_type = TypeSignature::parse_type_repr(key_type, &mut ())
            .map_err(|_| CheckErrors::BadMapTypeDefinition)?;
        let value_type = TypeSignature::parse_type_repr(value_type, &mut ())
            .map_err(|_| CheckErrors::BadMapTypeDefinition)?;

        Ok((map_name.clone(), (key_type, value_type)))
    }

    // Aaron: note, using lazy statics here would speed things up a bit and reduce clone()s
    fn try_native_function_check(
        &mut self,
        function: &str,
        args: &[SymbolicExpression],
        context: &TypingContext,
    ) -> Option<TypeResult> {
        if let Some(ref native_function) =
            NativeFunctions::lookup_by_name_at_version(function, &self.clarity_version)
        {
            let typed_function = TypedNativeFunction::type_native_function(native_function);
            Some(typed_function.type_check_appliction(self, args, context))
        } else {
            None
        }
    }

    fn type_check_function_application(
        &mut self,
        expression: &[SymbolicExpression],
        context: &TypingContext,
    ) -> TypeResult {
        let (function_name, args) = expression
            .split_first()
            .ok_or(CheckErrors::NonFunctionApplication)?;

        self.type_map.set_type(function_name, no_type())?;
        let function_name = function_name
            .match_atom()
            .ok_or(CheckErrors::NonFunctionApplication)?;

        if let Some(type_result) = self.try_native_function_check(function_name, args, context) {
            type_result
        } else {
            let function = match self.get_function_type(function_name) {
                Some(FunctionType::Fixed(function)) => Ok(function),
                _ => Err(CheckErrors::UnknownFunction(function_name.to_string())),
            }?;

            for (expected_type, found_type) in function.args.iter().map(|x| &x.signature).zip(args)
            {
                self.type_check_expects(found_type, context, &expected_type)?;
            }

            Ok(function.returns)
        }
    }

    fn lookup_variable(&mut self, name: &str, context: &TypingContext) -> TypeResult {
        runtime_cost(ClarityCostFunction::AnalysisLookupVariableConst, self, 0)?;

        if let Some(type_result) = type_reserved_variable(name, &self.clarity_version) {
            Ok(type_result)
        } else if let Some(type_result) = self.contract_context.get_variable_type(name) {
            Ok(type_result.clone())
        } else if let Some(type_result) = context.lookup_trait_reference_type(name) {
            Ok(TypeSignature::CallableType(CallableSubtype::Trait(
                type_result.clone(),
            )))
        } else {
            runtime_cost(
                ClarityCostFunction::AnalysisLookupVariableDepth,
                self,
                context.depth,
            )?;

            if let Some(type_result) = context.lookup_variable_type(name) {
                Ok(type_result.clone())
            } else {
                Err(CheckErrors::UndefinedVariable(name.to_string()).into())
            }
        }
    }

    fn clarity1_type_check_expects(
        &mut self,
        expr: &SymbolicExpression,
        context: &TypingContext,
        expected_type: &TypeSignature,
    ) -> TypeResult {
        match (&expr.expr, expected_type) {
            (
                LiteralValue(Value::Principal(PrincipalData::Contract(ref contract_identifier))),
                TypeSignature::CallableType(CallableSubtype::Trait(trait_identifier)),
            ) => {
                runtime_cost(
                    ClarityCostFunction::AnalysisFetchContractEntry,
                    &mut self.cost_track,
                    1,
                )?;
                let contract_to_check = self
                    .db
                    .load_contract(&contract_identifier)
                    .ok_or(CheckErrors::NoSuchContract(contract_identifier.to_string()))?;

                let trait_definition = match self.db.get_defined_trait(
                    &trait_identifier.contract_identifier,
                    &trait_identifier.name,
                ) {
                    Ok(Some(trait_sig)) => {
                        let type_size = trait_type_size(&trait_sig)?;
                        runtime_cost(
                            ClarityCostFunction::AnalysisUseTraitEntry,
                            &mut self.cost_track,
                            type_size,
                        )?;
                        trait_sig
                    }
                    Ok(None) => {
                        runtime_cost(
                            ClarityCostFunction::AnalysisUseTraitEntry,
                            &mut self.cost_track,
                            1,
                        )?;
                        return Err(CheckErrors::NoSuchTrait(
                            trait_identifier.contract_identifier.to_string(),
                            trait_identifier.name.to_string(),
                        )
                        .into());
                    }
                    Err(e) => {
                        runtime_cost(
                            ClarityCostFunction::AnalysisUseTraitEntry,
                            &mut self.cost_track,
                            1,
                        )?;
                        return Err(e);
                    }
                };

                contract_to_check.check_trait_compliance(trait_identifier, &trait_definition)?;
                return Ok(expected_type.clone());
            }
            (_, _) => {}
        }

        let actual_type = self.type_check(expr, context)?;
        analysis_typecheck_cost(self, expected_type, &actual_type)?;

        if !expected_type.admits_type(&actual_type) {
            let mut err: CheckError =
                CheckErrors::TypeError(expected_type.clone(), actual_type).into();
            err.set_expression(expr);
            Err(err)
        } else {
            Ok(actual_type)
        }
    }

    fn clarity2_type_check_expects(
        &mut self,
        expr: &SymbolicExpression,
        context: &TypingContext,
        expected_type: &TypeSignature,
    ) -> TypeResult {
        let mut expr_type = match expr.expr {
            AtomValue(ref value) | LiteralValue(ref value) => TypeSignature::type_of(value),
            Atom(ref name) => self.lookup_variable(name, context)?,
            List(ref expression) => self.type_check_function_application(expression, context)?,
            TraitReference(_, _) | Field(_) => {
                return Err(CheckErrors::UnexpectedTraitOrFieldReference.into());
            }
        };

        analysis_typecheck_cost(self, expected_type, &expr_type)?;
        match (expected_type, &expr.expr) {
            (
                TypeSignature::CallableType(CallableSubtype::Trait(expected_trait_id)),
                LiteralValue(Value::Principal(PrincipalData::Contract(ref contract_identifier))),
            ) => {
                // When a principal literal is used as a trait, make sure it implements the trait.
                let contract_to_check = self
                    .db
                    .load_contract(&contract_identifier)
                    .ok_or(CheckErrors::NoSuchContract(contract_identifier.to_string()))?;
                runtime_cost(
                    ClarityCostFunction::AnalysisFetchContractEntry,
                    &mut self.cost_track,
                    1,
                )?;

                let expected_trait = &lookup_trait(
                    self.db,
                    Some(&self.contract_context),
                    expected_trait_id,
                    self.clarity_version,
                    &mut self.cost_track,
                )?;
                contract_to_check.check_trait_compliance(expected_trait_id, expected_trait)?;
            }
            (TypeSignature::CallableType(CallableSubtype::Trait(expected_trait_id)), _) => {
                // When any other expression is used as a trait, those with trait
                // types should be checked for compatibility. Others should report
                // an error.
                let expr_trait_id =
                    if let TypeSignature::CallableType(CallableSubtype::Trait(expr_trait_id)) =
                        expr_type
                    {
                        expr_trait_id
                    } else {
                        return Err(CheckErrors::TypeError(expected_type.clone(), expr_type).into());
                    };
                let actual_trait = lookup_trait(
                    self.db,
                    Some(&self.contract_context),
                    &expr_trait_id,
                    self.clarity_version,
                    &mut self.cost_track,
                )?;
                let expected_trait = lookup_trait(
                    self.db,
                    Some(&self.contract_context),
                    expected_trait_id,
                    self.clarity_version,
                    &mut self.cost_track,
                )?;
                trait_check_trait_compliance(
                    self.db,
                    Some(&self.contract_context),
                    &expr_trait_id,
                    &actual_trait,
                    expected_trait_id,
                    &expected_trait,
                    self.clarity_version,
                    &mut self.cost_track,
                )?;
            }
            (_, _) => {
                inner_type_check_type(
                    self.db,
                    Some(&self.contract_context),
                    &expr_type,
                    expected_type,
                    self.clarity_version,
                    1,
                    &mut self.cost_track,
                )?;
            }
        }

        // If we reach here with no errors, then the expression can be
        // treated as the expected type.
        expr_type = expected_type.clone();

        runtime_cost(
            ClarityCostFunction::AnalysisTypeAnnotate,
            self,
            expr_type.type_size()?,
        )?;
        self.type_map.set_type(expr, expr_type.clone())?;
        Ok(expr_type)
    }

    fn inner_type_check(
        &mut self,
        expr: &SymbolicExpression,
        context: &TypingContext,
    ) -> TypeResult {
        let expr_type = match expr.expr {
            AtomValue(ref value) => TypeSignature::type_of(value),
            LiteralValue(ref value) => TypeSignature::literal_type_of(value),
            Atom(ref name) => self.lookup_variable(name, context)?,
            List(ref expression) => self.type_check_function_application(expression, context)?,
            TraitReference(_, _) | Field(_) => {
                return Err(CheckErrors::UnexpectedTraitOrFieldReference.into());
            }
        };

        runtime_cost(
            ClarityCostFunction::AnalysisTypeAnnotate,
            self,
            expr_type.type_size()?,
        )?;
        self.type_map.set_type(expr, expr_type.clone())?;
        Ok(expr_type)
    }

    fn type_check_define_variable(
        &mut self,
        var_name: &ClarityName,
        var_type: &SymbolicExpression,
        context: &mut TypingContext,
    ) -> CheckResult<(ClarityName, TypeSignature)> {
        let var_type = self.type_check(var_type, context)?;
        Ok((var_name.clone(), var_type))
    }

    fn type_check_define_persisted_variable(
        &mut self,
        var_name: &ClarityName,
        var_type: &SymbolicExpression,
        initial: &SymbolicExpression,
        context: &mut TypingContext,
    ) -> CheckResult<(ClarityName, TypeSignature)> {
        let expected_type = TypeSignature::parse_type_repr::<()>(var_type, &mut ())
            .map_err(|_e| CheckErrors::DefineVariableBadSignature)?;

        self.type_check_expects(initial, context, &expected_type)?;

        Ok((var_name.clone(), expected_type))
    }

    fn type_check_define_ft(
        &mut self,
        token_name: &ClarityName,
        bound: Option<&SymbolicExpression>,
        context: &mut TypingContext,
    ) -> CheckResult<ClarityName> {
        if let Some(bound) = bound {
            self.type_check_expects(bound, context, &TypeSignature::UIntType)?;
        }

        Ok(token_name.clone())
    }

    fn type_check_define_nft(
        &mut self,
        asset_name: &ClarityName,
        nft_type: &SymbolicExpression,
        _context: &mut TypingContext,
    ) -> CheckResult<(ClarityName, TypeSignature)> {
        let asset_type = TypeSignature::parse_type_repr::<()>(&nft_type, &mut ())
            .or_else(|_| Err(CheckErrors::DefineNFTBadSignature))?;

        Ok((asset_name.clone(), asset_type))
    }

    fn type_check_define_trait(
        &mut self,
        trait_name: &ClarityName,
        function_types: &[SymbolicExpression],
        _context: &mut TypingContext,
    ) -> CheckResult<(ClarityName, BTreeMap<ClarityName, FunctionSignature>)> {
        let trait_signature =
            TypeSignature::parse_trait_type_repr(&function_types, &mut (), self.clarity_version)?;

        Ok((trait_name.clone(), trait_signature))
    }

    // Checks if an expression is a _define_ expression, and if so, typechecks it. Otherwise, it returns Ok(None)
    fn try_type_check_define(
        &mut self,
        expression: &SymbolicExpression,
        context: &mut TypingContext,
    ) -> CheckResult<Option<()>> {
        if let Some(define_type) = DefineFunctionsParsed::try_parse(expression)? {
            match define_type {
                DefineFunctionsParsed::Constant { name, value } => {
                    let (v_name, v_type) = self.type_check_define_variable(name, value, context)?;
                    runtime_cost(
                        ClarityCostFunction::AnalysisBindName,
                        self,
                        v_type.type_size()?,
                    )?;
                    self.contract_context.add_variable_type(v_name, v_type)?;
                }
                DefineFunctionsParsed::PrivateFunction { signature, body } => {
                    let (f_name, f_type) =
                        self.type_check_define_function(signature, body, context)?;

                    runtime_cost(
                        ClarityCostFunction::AnalysisBindName,
                        self,
                        f_type.total_type_size()?,
                    )?;
                    self.contract_context
                        .add_private_function_type(f_name, FunctionType::Fixed(f_type))?;
                }
                DefineFunctionsParsed::PublicFunction { signature, body } => {
                    let (f_name, f_type) =
                        self.type_check_define_function(signature, body, context)?;
                    runtime_cost(
                        ClarityCostFunction::AnalysisBindName,
                        self,
                        f_type.total_type_size()?,
                    )?;

                    if f_type.returns.is_response_type() {
                        self.contract_context
                            .add_public_function_type(f_name, FunctionType::Fixed(f_type))?;
                        return Ok(Some(()));
                    } else {
                        return Err(
                            CheckErrors::PublicFunctionMustReturnResponse(f_type.returns).into(),
                        );
                    }
                }
                DefineFunctionsParsed::ReadOnlyFunction { signature, body } => {
                    let (f_name, f_type) =
                        self.type_check_define_function(signature, body, context)?;
                    runtime_cost(
                        ClarityCostFunction::AnalysisBindName,
                        self,
                        f_type.total_type_size()?,
                    )?;
                    self.contract_context
                        .add_read_only_function_type(f_name, FunctionType::Fixed(f_type))?;
                }
                DefineFunctionsParsed::Map {
                    name,
                    key_type,
                    value_type,
                } => {
                    let (f_name, map_type) =
                        self.type_check_define_map(name, key_type, value_type)?;
                    let total_type_size = u64::from(map_type.0.type_size()?)
                        .cost_overflow_add(u64::from(map_type.1.type_size()?))?;
                    runtime_cost(ClarityCostFunction::AnalysisBindName, self, total_type_size)?;
                    self.contract_context.add_map_type(f_name, map_type)?;
                }
                DefineFunctionsParsed::PersistedVariable {
                    name,
                    data_type,
                    initial,
                } => {
                    let (v_name, v_type) = self
                        .type_check_define_persisted_variable(name, data_type, initial, context)?;
                    runtime_cost(
                        ClarityCostFunction::AnalysisBindName,
                        self,
                        v_type.type_size()?,
                    )?;
                    self.contract_context
                        .add_persisted_variable_type(v_name, v_type)?;
                }
                DefineFunctionsParsed::BoundedFungibleToken { name, max_supply } => {
                    let token_name = self.type_check_define_ft(name, Some(max_supply), context)?;
                    runtime_cost(
                        ClarityCostFunction::AnalysisBindName,
                        self,
                        TypeSignature::UIntType.type_size()?,
                    )?;
                    self.contract_context.add_ft(token_name)?;
                }
                DefineFunctionsParsed::UnboundedFungibleToken { name } => {
                    let token_name = self.type_check_define_ft(name, None, context)?;
                    runtime_cost(
                        ClarityCostFunction::AnalysisBindName,
                        self,
                        TypeSignature::UIntType.type_size()?,
                    )?;
                    self.contract_context.add_ft(token_name)?;
                }
                DefineFunctionsParsed::NonFungibleToken { name, nft_type } => {
                    let (token_name, token_type) =
                        self.type_check_define_nft(name, nft_type, context)?;
                    runtime_cost(
                        ClarityCostFunction::AnalysisBindName,
                        self,
                        token_type.type_size()?,
                    )?;
                    self.contract_context.add_nft(token_name, token_type)?;
                }
                DefineFunctionsParsed::Trait { name, functions } => {
                    let (trait_name, trait_signature) =
                        self.type_check_define_trait(name, functions, context)?;
                    runtime_cost(
                        ClarityCostFunction::AnalysisBindName,
                        self,
                        trait_type_size(&trait_signature)?,
                    )?;
                    if self.clarity_version < ClarityVersion::Clarity2 {
                        self.contract_context
                            .add_trait(trait_name, trait_signature)?;
                    } else {
                        self.contract_context
                            .add_defined_trait(name.clone(), trait_signature)?;
                    }
                }
                DefineFunctionsParsed::UseTrait {
                    name,
                    trait_identifier,
                } => {
                    let result = self.db.get_defined_trait(
                        &trait_identifier.contract_identifier,
                        &trait_identifier.name,
                    )?;
                    match result {
                        Some(trait_sig) => {
                            let type_size = trait_type_size(&trait_sig)?;
                            runtime_cost(
                                ClarityCostFunction::AnalysisUseTraitEntry,
                                self,
                                type_size,
                            )?;
                            runtime_cost(ClarityCostFunction::AnalysisBindName, self, type_size)?;
                            if self.clarity_version < ClarityVersion::Clarity2 {
                                self.contract_context
                                    .add_trait(trait_identifier.name.clone(), trait_sig)?
                            } else {
                                self.contract_context
                                    .add_used_trait(trait_identifier.clone(), trait_sig)?
                            }
                        }
                        None => {
                            // still had to do a db read, even if it didn't exist!
                            runtime_cost(ClarityCostFunction::AnalysisUseTraitEntry, self, 1)?;
                            return Err(CheckErrors::TraitReferenceUnknown(name.to_string()).into());
                        }
                    }
                }
                DefineFunctionsParsed::ImplTrait { trait_identifier } => {
                    self.contract_context
                        .add_implemented_trait(trait_identifier.clone())?;
                }
            };
            Ok(Some(()))
        } else {
            // not a define.
            Ok(None)
        }
    }
}
