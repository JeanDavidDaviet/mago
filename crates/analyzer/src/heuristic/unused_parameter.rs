use mago_codex::metadata::function_like::FunctionLikeMetadata;
use mago_fixer::SafetyClassification;
use mago_reporting::Annotation;
use mago_reporting::Issue;
use mago_span::HasSpan;
use mago_span::Span;
use mago_syntax::ast::FunctionLikeParameter;

use crate::code::IssueCode;
use crate::context::Context;
use crate::heuristic::unused_parameter::utils::expression_potentially_contains_function_call;
use crate::heuristic::unused_parameter::utils::get_foreign_variable_names;
use crate::heuristic::unused_parameter::utils::is_variable_used_in_expression;
use crate::statement::function_like::FunctionLikeBody;

const FUNC_GET_ARGS: &str = "func_get_args";

pub fn check_unused_params(
    metadata: &FunctionLikeMetadata,
    params: &[FunctionLikeParameter],
    body: FunctionLikeBody,
    ctx: &mut Context<'_>,
) {
    match body {
        FunctionLikeBody::Statements(statements) => {
            if utils::potentially_contains_function_call(statements, FUNC_GET_ARGS, ctx) {
                // `func_get_args` is used, so we can't determine if the parameters are unused in this case
                return;
            }

            let foreign_variables = get_foreign_variable_names(statements, ctx);

            for param in params {
                if param.is_promoted_property() {
                    // Skip promoted properties
                    continue;
                }

                if foreign_variables.iter().any(|v| v.name == param.variable.name) {
                    continue;
                }

                report_parameter(
                    param,
                    metadata.span,
                    ctx,
                    if metadata.kind.is_closure() {
                        "closure"
                    } else if metadata.kind.is_method() {
                        "method"
                    } else {
                        "function"
                    },
                );
            }
        }
        FunctionLikeBody::Expression(expression) => {
            if expression_potentially_contains_function_call(expression, FUNC_GET_ARGS, ctx) {
                // `func_get_args` is used, so we can't determine if the parameters are unused in this case
                return;
            }

            for param in params {
                if !is_variable_used_in_expression(expression, ctx, param.variable.name) {
                    report_parameter(param, metadata.span, ctx, "arrow function");
                }
            }
        }
    }
}

fn report_parameter(
    parameter: &FunctionLikeParameter,
    function_like: Span,
    context: &mut Context<'_>,
    kind: &'static str,
) {
    if parameter.ampersand.is_some() {
        return;
    }

    let parameter_name = context.interner.lookup(&parameter.variable.name);
    if parameter_name.starts_with("$_") {
        return;
    }

    let issue = Issue::help(format!("Parameter `{parameter_name}` is never used."))
        .with_code(IssueCode::UnusedParameter)
        .with_annotations([
            Annotation::primary(parameter.span()).with_message(format!("Parameter `{parameter_name}` is declared here.")),
            Annotation::secondary(function_like),
        ])
        .with_note(format!("This parameter is declared but not used within the {kind}."))
        .with_help("Consider prefixing the parameter with an underscore (`_`) to indicate that it is intentionally unused, or remove it if it is not needed.");

    context.collector.propose(issue, |plan| {
        plan.insert(
            parameter.variable.span().start.offset + 1, // skip the leading `$`
            "_",
            SafetyClassification::PotentiallyUnsafe,
        );
    });
}

mod utils {
    use ahash::HashSet;
    use mago_interner::StringIdentifier;
    use mago_syntax::ast::*;
    use mago_syntax::walker::Walker;

    use crate::context::Context;
    use crate::heuristic::unused_parameter::utils::internal::FunctionCallWalker;

    #[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, PartialOrd, Ord)]
    pub struct ForeignVariable {
        pub name: StringIdentifier,
        pub conditionally: bool,
    }

    pub fn is_super_global_variable(name: &str) -> bool {
        matches!(
            name,
            "$_GET" | "$_POST" | "$_COOKIE" | "$_REQUEST" | "$_SERVER" | "$_FILES" | "$_ENV" | "$_SESSION" | "$GLOBALS"
        )
    }

    pub fn is_predefined_variable(name: &str) -> bool {
        is_super_global_variable(name) || "$this" == name
    }

    pub fn potentially_contains_function_call(block: &[Statement], function_name: &str, context: &Context<'_>) -> bool {
        let mut context = (false, context);

        let walker = FunctionCallWalker(function_name);
        for stmt in block {
            if context.0 {
                break;
            }

            walker.walk_statement(stmt, &mut context);
        }

        context.0
    }

    pub fn expression_potentially_contains_function_call(
        expression: &Expression,
        function_name: &str,
        context: &Context<'_>,
    ) -> bool {
        let mut context = (false, context);

        FunctionCallWalker(function_name).walk_expression(expression, &mut context);

        context.0
    }

    pub fn is_variable_used_in_expression(
        expression: &Expression,
        context: &Context<'_>,
        variable: StringIdentifier,
    ) -> bool {
        use crate::heuristic::unused_parameter::utils::internal::VariableReference;
        use crate::heuristic::unused_parameter::utils::internal::VariableWalker;

        let mut context = (Vec::default(), context, 0);

        VariableWalker.walk_expression(expression, &mut context);

        let variables = context.0;
        let mut reassigned = false;
        for variable_reference in variables {
            match variable_reference {
                VariableReference::Use(string_identifier) => {
                    if !reassigned && string_identifier == variable {
                        return true;
                    }
                }
                VariableReference::Assign(string_identifier, conditionally) => {
                    if !conditionally && string_identifier == variable {
                        reassigned = true;
                    }
                }
                VariableReference::Unset(string_identifier) => {
                    if string_identifier == variable {
                        if reassigned {
                            reassigned = false;
                        } else {
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    pub fn get_foreign_variable_names(stmts: &[Statement], context: &Context<'_>) -> Vec<ForeignVariable> {
        use internal::*;

        let mut walker_context = (Vec::default(), context, 0);
        let walker = VariableWalker;
        for stmt in stmts {
            walker.walk_statement(stmt, &mut walker_context);
        }

        let variable_references = walker_context.0;
        let mut definitely_assigned = HashSet::default();
        let mut conditionally_assigned = HashSet::default();
        let mut foreign = Vec::default();
        let mut foreign_names = HashSet::default();

        for reference in variable_references {
            match reference {
                VariableReference::Use(name) => {
                    if !definitely_assigned.contains(&name) && !foreign_names.contains(&name) {
                        let is_conditional = conditionally_assigned.contains(&name);
                        foreign.push(ForeignVariable { name, conditionally: is_conditional });
                        foreign_names.insert(name);
                    }
                }
                VariableReference::Assign(name, is_conditional) => {
                    if is_conditional {
                        conditionally_assigned.insert(name);
                    } else {
                        definitely_assigned.insert(name);
                        conditionally_assigned.remove(&name);
                    }
                }
                VariableReference::Unset(name) => {
                    definitely_assigned.remove(&name);
                    conditionally_assigned.remove(&name);
                }
            }
        }

        foreign
    }

    mod internal {
        use super::is_predefined_variable;

        use mago_interner::StringIdentifier;
        use mago_syntax::ast::*;
        use mago_syntax::walker::Walker;

        use crate::context::Context;

        #[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, PartialOrd, Ord)]
        pub(super) enum VariableReference {
            Use(StringIdentifier),
            Assign(StringIdentifier, bool),
            Unset(StringIdentifier),
        }

        #[derive(Debug)]
        pub(super) struct VariableWalker;

        #[derive(Debug)]
        pub(super) struct FunctionCallWalker<'a>(pub &'a str);

        impl<'a> Walker<(Vec<VariableReference>, &'a Context<'a>, usize)> for VariableWalker {
            fn walk_if<'ast>(&self, r#if: &'ast If, context: &mut (Vec<VariableReference>, &'a Context<'a>, usize)) {
                self.walk_expression(&r#if.condition, context);

                context.2 += 1;
                self.walk_if_body(&r#if.body, context);
                context.2 -= 1;
            }

            fn walk_for<'ast>(&self, r#for: &'ast For, context: &mut (Vec<VariableReference>, &'a Context<'a>, usize)) {
                for i in r#for.initializations.iter() {
                    self.walk_expression(i, context);
                }

                for c in r#for.conditions.iter() {
                    self.walk_expression(c, context);
                }

                for i in r#for.increments.iter() {
                    self.walk_expression(i, context);
                }

                context.2 += 1;
                self.walk_for_body(&r#for.body, context);
                context.2 -= 1;
            }

            fn walk_while<'ast>(
                &self,
                r#while: &'ast While,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                self.walk_expression(&r#while.condition, context);
                context.2 += 1;
                self.walk_while_body(&r#while.body, context);
                context.2 -= 1;
            }

            fn walk_do_while<'ast>(
                &self,
                do_while: &'ast DoWhile,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                context.2 += 1;
                self.walk_statement(&do_while.statement, context);
                context.2 -= 1;
                self.walk_expression(&do_while.condition, context);
            }

            fn walk_match_expression_arm(
                &self,
                match_expression_arm: &MatchExpressionArm,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                for c in match_expression_arm.conditions.iter() {
                    self.walk_expression(c, context);
                }

                context.2 += 1;
                self.walk_expression(&match_expression_arm.expression, context);
                context.2 -= 1;
            }

            fn walk_match_default_arm(
                &self,
                match_default_arm: &MatchDefaultArm,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                context.2 += 1;
                self.walk_expression(&match_default_arm.expression, context);
                context.2 -= 1;
            }

            fn walk_switch_expression_case(
                &self,
                switch_expression_case: &SwitchExpressionCase,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                self.walk_expression(&switch_expression_case.expression, context);
                context.2 += 1;
                for statement in switch_expression_case.statements.iter() {
                    self.walk_statement(statement, context);
                }
                context.2 -= 1;
            }

            fn walk_switch_default_case<'ast>(
                &self,
                switch_default_case: &'ast SwitchDefaultCase,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                context.2 += 1;
                for statement in switch_default_case.statements.iter() {
                    self.walk_statement(statement, context);
                }
                context.2 -= 1;
            }

            fn walk_in_try_catch_clause<'ast>(
                &self,
                try_catch_clause: &'ast TryCatchClause,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                if let Some(variable) = &try_catch_clause.variable {
                    context.0.push(VariableReference::Assign(variable.name, true));
                }

                context.2 += 1;
            }

            fn walk_out_try_catch_clause<'ast>(
                &self,
                _try_catch_clause: &'ast TryCatchClause,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                context.2 -= 1;
            }

            fn walk_in_foreach_value_target<'ast>(
                &self,
                foreach_value_target: &'ast ForeachValueTarget,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                scan_expression_for_assignment(&foreach_value_target.value, context.1, &mut context.0, true);
            }

            fn walk_in_foreach_key_value_target<'ast>(
                &self,
                foreach_key_value_target: &'ast ForeachKeyValueTarget,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                scan_expression_for_assignment(&foreach_key_value_target.key, context.1, &mut context.0, true);
                scan_expression_for_assignment(&foreach_key_value_target.value, context.1, &mut context.0, true);
            }

            fn walk_in_static_concrete_item<'ast>(
                &self,
                static_concrete_item: &'ast StaticConcreteItem,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                context.0.push(VariableReference::Assign(static_concrete_item.variable.name, context.2 > 0));
            }

            fn walk_in_static_abstract_item<'ast>(
                &self,
                static_abstract_item: &'ast StaticAbstractItem,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                context.0.push(VariableReference::Assign(static_abstract_item.variable.name, context.2 > 0));
            }

            fn walk_in_global<'ast>(
                &self,
                global: &'ast Global,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                for variable in global.variables.iter() {
                    let Variable::Direct(variable) = variable else {
                        continue;
                    };
                    context.0.push(VariableReference::Assign(variable.name, context.2 > 0));
                }
            }

            fn walk_conditional(
                &self,
                conditional: &Conditional,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                self.walk_expression(&conditional.condition, context);

                context.2 += 1;
                if let Some(expr) = conditional.then.as_deref() {
                    self.walk_expression(expr, context);
                }
                self.walk_expression(&conditional.r#else, context);
                context.2 -= 1;
            }

            fn walk_binary(&self, binary: &Binary, context: &mut (Vec<VariableReference>, &'a Context<'a>, usize)) {
                self.walk_expression(&binary.lhs, context);

                if !binary.operator.is_elvis() && !binary.operator.is_null_coalesce() && !binary.operator.is_logical() {
                    self.walk_expression(&binary.rhs, context);
                    return;
                };

                context.2 += 1;
                self.walk_expression(&binary.rhs, context);
                context.2 -= 1;
            }

            fn walk_assignment<'ast>(
                &self,
                assignment: &'ast Assignment,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                self.walk_expression(&assignment.rhs, context);

                let is_conditional = context.2 > 0;
                let mut variables = Vec::default();
                scan_expression_for_assignment(&assignment.lhs, context.1, &mut variables, is_conditional);

                match assignment.operator {
                    AssignmentOperator::Assign(_) => {
                        context.0.extend(variables);
                    }
                    _ => {
                        for variable in variables {
                            if let VariableReference::Assign(name, is_cond) = variable {
                                context.0.push(VariableReference::Use(name));
                                context.0.push(VariableReference::Assign(name, is_cond));
                            } else {
                                context.0.push(variable);
                            }
                        }
                    }
                }

                self.walk_expression(&assignment.lhs, context);
            }

            fn walk_in_direct_variable<'ast>(
                &self,
                direct_variable: &'ast DirectVariable,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                let name = context.1.interner.lookup(&direct_variable.name);
                if !is_predefined_variable(name) {
                    context.0.push(VariableReference::Use(direct_variable.name));
                }
            }

            fn walk_closure<'ast>(
                &self,
                closure: &'ast Closure,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                if let Some(use_clause) = &closure.use_clause {
                    for use_clause_variable in use_clause.variables.iter() {
                        context.0.push(VariableReference::Use(use_clause_variable.variable.name));
                    }
                }
            }

            fn walk_in_arrow_function<'ast>(
                &self,
                arrow_function: &'ast ArrowFunction,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                for parameter in arrow_function.parameter_list.parameters.iter() {
                    context.0.push(VariableReference::Assign(parameter.variable.name, false));
                }
            }

            fn walk_out_arrow_function<'ast>(
                &self,
                arrow_function: &'ast ArrowFunction,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                for parameter in arrow_function.parameter_list.parameters.iter() {
                    context.0.push(VariableReference::Unset(parameter.variable.name));
                }
            }

            #[inline]
            fn walk_anonymous_class<'ast>(
                &self,
                anonymous_class: &'ast AnonymousClass,
                context: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
                if let Some(argument_list) = anonymous_class.argument_list.as_ref() {
                    self.walk_argument_list(argument_list, context);
                }
            }

            #[inline]
            fn walk_namespace<'ast>(
                &self,
                _: &'ast Namespace,
                _: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
            }

            #[inline]
            fn walk_class<'ast>(&self, _: &'ast Class, _: &mut (Vec<VariableReference>, &'a Context<'a>, usize)) {}

            #[inline]
            fn walk_interface<'ast>(
                &self,
                _: &'ast Interface,
                _: &mut (Vec<VariableReference>, &'a Context<'a>, usize),
            ) {
            }

            #[inline]
            fn walk_trait<'ast>(&self, _: &'ast Trait, _: &mut (Vec<VariableReference>, &'a Context<'a>, usize)) {}

            #[inline]
            fn walk_enum<'ast>(&self, _: &'ast Enum, _: &mut (Vec<VariableReference>, &'a Context<'a>, usize)) {}

            #[inline]
            fn walk_function<'ast>(&self, _: &'ast Function, _: &mut (Vec<VariableReference>, &'a Context<'a>, usize)) {
            }
        }

        impl<'a> Walker<(bool, &'a Context<'a>)> for FunctionCallWalker<'a> {
            fn walk_in_function_call<'ast>(
                &self,
                function_call: &'ast FunctionCall,
                context: &mut (bool, &'a Context<'a>),
            ) {
                if context.0 {
                    return;
                }

                let Expression::Identifier(func) = function_call.function.as_ref() else {
                    return;
                };

                context.0 = context.1.resolve_function_name(func).eq_ignore_ascii_case(self.0);
            }

            #[inline]
            fn walk_closure<'ast>(&self, _: &'ast Closure, _: &mut (bool, &'a Context<'a>)) {}

            #[inline]
            fn walk_arrow_function<'ast>(&self, _: &'ast ArrowFunction, _: &mut (bool, &'a Context<'a>)) {}

            #[inline]
            fn walk_namespace<'ast>(&self, _: &'ast Namespace, _: &mut (bool, &'a Context<'a>)) {}

            #[inline]
            fn walk_class<'ast>(&self, _: &'ast Class, _: &mut (bool, &'a Context<'a>)) {}

            #[inline]
            fn walk_interface<'ast>(&self, _: &'ast Interface, _: &mut (bool, &'a Context<'a>)) {}

            #[inline]
            fn walk_trait<'ast>(&self, _: &'ast Trait, _: &mut (bool, &'a Context<'a>)) {}

            #[inline]
            fn walk_enum<'ast>(&self, _: &'ast Enum, _: &mut (bool, &'a Context<'a>)) {}

            #[inline]
            fn walk_function<'ast>(&self, _: &'ast Function, _: &mut (bool, &'a Context<'a>)) {}

            #[inline]
            fn walk_anonymous_class<'ast>(&self, _: &'ast AnonymousClass, _: &mut (bool, &'a Context<'a>)) {}
        }

        fn scan_expression_for_assignment(
            expression: &Expression,
            context: &Context<'_>,
            variables: &mut Vec<VariableReference>,
            is_conditional: bool,
        ) {
            match &expression {
                Expression::Variable(variable) => {
                    let Variable::Direct(variable) = variable else {
                        return;
                    };

                    let name = context.interner.lookup(&variable.name);
                    if !is_predefined_variable(name) {
                        variables.push(VariableReference::Assign(variable.name, is_conditional));
                    }
                }
                Expression::Array(array) => {
                    for element in array.elements.iter() {
                        match &element {
                            ArrayElement::KeyValue(key_value_array_element) => {
                                scan_expression_for_assignment(
                                    &key_value_array_element.key,
                                    context,
                                    variables,
                                    is_conditional,
                                );
                                scan_expression_for_assignment(
                                    &key_value_array_element.value,
                                    context,
                                    variables,
                                    is_conditional,
                                );
                            }
                            ArrayElement::Value(value_array_element) => {
                                scan_expression_for_assignment(
                                    &value_array_element.value,
                                    context,
                                    variables,
                                    is_conditional,
                                );
                            }
                            _ => {}
                        }
                    }
                }
                Expression::LegacyArray(array) => {
                    for element in array.elements.iter() {
                        match &element {
                            ArrayElement::KeyValue(key_value_array_element) => {
                                scan_expression_for_assignment(
                                    &key_value_array_element.key,
                                    context,
                                    variables,
                                    is_conditional,
                                );
                                scan_expression_for_assignment(
                                    &key_value_array_element.value,
                                    context,
                                    variables,
                                    is_conditional,
                                );
                            }
                            ArrayElement::Value(value_array_element) => {
                                scan_expression_for_assignment(
                                    &value_array_element.value,
                                    context,
                                    variables,
                                    is_conditional,
                                );
                            }
                            _ => {}
                        }
                    }
                }
                Expression::List(list) => {
                    for element in list.elements.iter() {
                        match &element {
                            ArrayElement::KeyValue(key_value_array_element) => {
                                scan_expression_for_assignment(
                                    &key_value_array_element.key,
                                    context,
                                    variables,
                                    is_conditional,
                                );
                                scan_expression_for_assignment(
                                    &key_value_array_element.value,
                                    context,
                                    variables,
                                    is_conditional,
                                );
                            }
                            ArrayElement::Value(value_array_element) => {
                                scan_expression_for_assignment(
                                    &value_array_element.value,
                                    context,
                                    variables,
                                    is_conditional,
                                );
                            }
                            _ => {}
                        }
                    }
                }
                Expression::ArrayAppend(append) => {
                    if let Expression::Variable(Variable::Direct(variable)) = append.array.as_ref() {
                        let name = context.interner.lookup(&variable.name);
                        if !is_predefined_variable(name) {
                            variables.push(VariableReference::Use(variable.name));
                        }
                    }

                    scan_expression_for_assignment(&append.array, context, variables, is_conditional);
                }
                Expression::ArrayAccess(access) => {
                    if let Expression::Variable(Variable::Direct(variable)) = access.array.as_ref() {
                        let name = context.interner.lookup(&variable.name);
                        if !is_predefined_variable(name) {
                            variables.push(VariableReference::Use(variable.name));
                        }
                    }

                    if let Expression::Variable(Variable::Direct(variable)) = access.index.as_ref() {
                        let name = context.interner.lookup(&variable.name);
                        if !is_predefined_variable(name) {
                            variables.push(VariableReference::Use(variable.name));
                        }
                    }

                    scan_expression_for_assignment(&access.array, context, variables, is_conditional);
                    scan_expression_for_assignment(&access.index, context, variables, is_conditional);
                }
                _ => {}
            }
        }
    }
}
