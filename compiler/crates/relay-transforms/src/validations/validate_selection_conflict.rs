/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::Arc;

use crate::{PointerAddress, ValidationMessage};

use common::{Diagnostic, DiagnosticsResult, Location};
use dashmap::DashMap;
use errors::{par_try_map, validate, validate_map};
use graphql_ir::{
    FragmentDefinition, LinkedField, OperationDefinition, Program, ScalarField, Selection,
};
use interner::StringKey;
use schema::{SDLSchema, Schema, Type};

pub fn validate_selection_conflict(program: &Program) -> DiagnosticsResult<()> {
    ValidateSelectionConflict::new(program).validate_program(program)
}

#[derive(Clone, PartialEq)]
enum Field<'s> {
    LinkedField(&'s LinkedField),
    ScalarFeild(&'s ScalarField),
}

type Fields<'s> = Vec<Field<'s>>;

struct ValidateSelectionConflict<'s> {
    program: &'s Program,
    fragment_cache: DashMap<StringKey, Arc<Fields<'s>>>,
    fields_cache: DashMap<PointerAddress, Arc<Fields<'s>>>,
}

impl<'s> ValidateSelectionConflict<'s> {
    fn new(program: &'s Program) -> Self {
        Self {
            program,
            fragment_cache: Default::default(),
            fields_cache: Default::default(),
        }
    }

    fn validate_program(&self, program: &'s Program) -> DiagnosticsResult<()> {
        validate!(
            par_try_map(&program.operations, |operation| {
                self.validate_operation(operation)
            }),
            par_try_map(&program.fragments, |(_, fragment)| {
                self.validate_and_collect_fragment(fragment)
            })
        )
    }

    fn validate_operation(&self, operation: &'s OperationDefinition) -> DiagnosticsResult<()> {
        self.validate_selections(&operation.selections)?;
        Ok(())
    }

    fn validate_selections(&self, selections: &'s [Selection]) -> DiagnosticsResult<Fields<'s>> {
        let mut fields = Vec::new();
        validate_map(selections, |selection| {
            self.validate_selection(&mut fields, selection)
        })?;
        Ok(fields)
    }

    fn validate_selection(
        &self,
        fields: &mut Fields<'s>,
        selection: &'s Selection,
    ) -> DiagnosticsResult<()> {
        match selection {
            Selection::LinkedField(field) => {
                self.validate_linked_field_selections(field)?;
                let field = Field::LinkedField(field.as_ref());
                self.validate_and_insert_field_selection(fields, field, false)
            }
            Selection::ScalarField(field) => {
                let field = Field::ScalarFeild(field.as_ref());
                self.validate_and_insert_field_selection(fields, field, false)
            }
            Selection::Condition(condition) => {
                let new_fields = self.validate_selections(&condition.selections)?;
                self.validate_and_merge_fields(fields, new_fields, false)
            }
            Selection::InlineFragment(fragment) => {
                let new_fields = self.validate_selections(&fragment.selections)?;
                self.validate_and_merge_fields(fields, new_fields, false)
            }
            Selection::FragmentSpread(spread) => {
                let fragment = self.program.fragment(spread.fragment.item).unwrap();
                let new_fields = self.validate_and_collect_fragment(fragment)?;
                self.validate_and_merge_fields(fields, new_fields.to_vec(), false)
            }
        }
    }

    fn validate_and_collect_fragment(
        &self,
        fragment: &'s FragmentDefinition,
    ) -> DiagnosticsResult<Arc<Fields<'s>>> {
        if let Some(cached) = self.fragment_cache.get(&fragment.name.item) {
            return Ok(Arc::clone(&cached));
        }
        let fields = Arc::new(self.validate_selections(&fragment.selections)?);
        self.fragment_cache
            .insert(fragment.name.item, Arc::clone(&fields));
        Ok(fields)
    }

    fn validate_linked_field_selections(
        &self,
        field: &'s LinkedField,
    ) -> DiagnosticsResult<Arc<Fields<'s>>> {
        let key = PointerAddress::new(field);
        if let Some(fields) = self.fields_cache.get(&key) {
            return Ok(Arc::clone(&fields));
        }
        let fields = Arc::new(self.validate_selections(&field.selections)?);
        self.fields_cache.insert(key, Arc::clone(&fields));
        Ok(fields)
    }

    fn validate_and_merge_fields(
        &self,
        left: &mut Fields<'s>,
        right: Fields<'s>,
        parent_fields_mutually_exclusive: bool,
    ) -> DiagnosticsResult<()> {
        validate_map(right, |field| {
            self.validate_and_insert_field_selection(left, field, parent_fields_mutually_exclusive)
        })
    }

    fn validate_and_insert_field_selection(
        &self,
        fields: &mut Fields<'s>,
        field: Field<'s>,
        parent_fields_mutually_exclusive: bool,
    ) -> DiagnosticsResult<()> {
        let key = field.get_response_key(&self.program.schema);
        let mut errors = vec![];

        for existing_field in fields
            .iter_mut()
            .filter(|field| key == field.get_response_key(&self.program.schema))
        {
            if &field == existing_field {
                return if errors.is_empty() {
                    Ok(())
                } else {
                    Err(errors)
                };
            }

            let l_definition = existing_field.get_field_definition(&self.program.schema);
            let r_definition = field.get_field_definition(&self.program.schema);

            let is_parent_fields_mutually_exclusive = || {
                parent_fields_mutually_exclusive
                    || l_definition.parent_type != r_definition.parent_type
                        && matches!(
                            (l_definition.parent_type, r_definition.parent_type),
                            (Some(Type::Object(_)), Some(Type::Object(_)))
                        )
            };

            match (existing_field, &field) {
                (Field::LinkedField(l), Field::LinkedField(r)) => {
                    let fields_mutually_exclusive = is_parent_fields_mutually_exclusive();
                    if !fields_mutually_exclusive {
                        if l_definition.name != r_definition.name {
                            errors.push(
                                Diagnostic::error(
                                    ValidationMessage::AmbiguousFieldAlias {
                                        response_key: key,
                                        l_name: l_definition.name,
                                        r_name: r_definition.name,
                                    },
                                    l.definition.location,
                                )
                                .annotate("the other field", r.definition.location),
                            );
                        }
                    }
                    let mut l_fields = self.validate_linked_field_selections(&l)?;
                    let r_fields = self.validate_linked_field_selections(&r)?;

                    if let Err(errs) = self.validate_and_merge_fields(
                        Arc::make_mut(&mut l_fields),
                        r_fields.to_vec(),
                        fields_mutually_exclusive,
                    ) {
                        errors.extend(errs);
                    }
                }
                (Field::ScalarFeild(l), Field::ScalarFeild(r)) => {
                    if !is_parent_fields_mutually_exclusive() {
                        if l_definition.name != r_definition.name {
                            errors.push(
                                Diagnostic::error(
                                    ValidationMessage::AmbiguousFieldAlias {
                                        response_key: key,
                                        l_name: l_definition.name,
                                        r_name: r_definition.name,
                                    },
                                    l.definition.location,
                                )
                                .annotate("the other field", r.definition.location),
                            );
                        }
                    } else if l_definition.type_ != r_definition.type_ {
                        errors.push(
                            Diagnostic::error(
                                ValidationMessage::AmbiguousFieldType {
                                    response_key: key,
                                    l_name: l_definition.name,
                                    r_name: r_definition.name,
                                    l_type_string: self
                                        .program
                                        .schema
                                        .get_type_string(&l_definition.type_),
                                    r_type_string: self
                                        .program
                                        .schema
                                        .get_type_string(&r_definition.type_),
                                },
                                l.definition.location,
                            )
                            .annotate("the other field", field.loc()),
                        );
                    }
                }
                (existing_field, _) => {
                    errors.push(
                        Diagnostic::error(
                            ValidationMessage::AmbiguousFieldType {
                                response_key: key,
                                l_name: l_definition.name,
                                r_name: r_definition.name,
                                l_type_string: self
                                    .program
                                    .schema
                                    .get_type_string(&l_definition.type_),
                                r_type_string: self
                                    .program
                                    .schema
                                    .get_type_string(&r_definition.type_),
                            },
                            existing_field.loc(),
                        )
                        .annotate("the other field", field.loc()),
                    );
                }
            }
        }
        if errors.is_empty() {
            fields.push(field);
            Ok(())
        } else {
            Err(errors)
        }
    }
}

impl<'s> Field<'s> {
    fn get_response_key(&self, schema: &SDLSchema) -> StringKey {
        match self {
            Field::LinkedField(f) => f.alias_or_name(schema),
            Field::ScalarFeild(f) => f.alias_or_name(schema),
        }
    }

    fn get_field_definition(&self, schema: &'s SDLSchema) -> &'s schema::definitions::Field {
        match self {
            Field::LinkedField(f) => schema.field(f.definition.item),
            Field::ScalarFeild(f) => schema.field(f.definition.item),
        }
    }

    fn loc(&self) -> Location {
        match self {
            Field::LinkedField(f) => f.definition.location,
            Field::ScalarFeild(f) => f.definition.location,
        }
    }
}