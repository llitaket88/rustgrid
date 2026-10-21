//! Oracle table introspection (`ALL_*` catalogs) and `CREATE`/`ALTER TABLE` generation for the
//! table designer. Oracle has no `SHOW CREATE`, so the structured catalogs are the source of truth.

use std::collections::HashMap;

use rustgrid_core::{
    CheckDef, ColumnDef, ForeignKeyDef, IndexColumnDef, IndexDef, Result, TableOptions,
    TableSchema, TriggerDef, UniqueKeyDef,
};

use crate::connection::{OracleConnection, map_query_error};
use crate::helpers::{qualify, quote_identifier, quote_literal};

/// Split a catalog `data_type` (e.g. `NUMBER`, `VARCHAR2`, `TIMESTAMP(6)`) into a designer base
/// type. The length/decimals come from the separate precision/scale/length columns.
fn base_type(data_type: &str) -> String {
    let upper = data_type.trim().to_ascii_uppercase();
    if upper.starts_with("TIMESTAMP") {
        return "TIMESTAMP".to_string();
    }
    match data_type.split_once('(') {
        Some((head, _)) => head.trim().to_string(),
        None => data_type.trim().to_string(),
    }
}

/// Render one column definition for `CREATE`/`ALTER TABLE`.
fn column_sql(column: &ColumnDef) -> String {
    let mut sql = quote_identifier(&column.name);
    let data_type = column.data_type.trim();
    let data_type = if data_type.is_empty() {
        "VARCHAR2"
    } else {
        data_type
    };
    sql.push(' ');
    sql.push_str(data_type);

    let length = column.length.trim();
    let decimals = column.decimals.trim();
    if !length.is_empty() && length != "0" {
        sql.push('(');
        sql.push_str(length);
        if !decimals.is_empty() && decimals != "0" {
            sql.push(',');
            sql.push_str(decimals);
        }
        sql.push(')');
    }

    if column.auto_increment {
        // A served `GENERATED ALWAYS` identity must stay `ALWAYS`; rewriting it to `BY DEFAULT`
        // would change the column's insert semantics on the next designer save.
        let generation = if column.identity.trim().eq_ignore_ascii_case("ALWAYS") {
            "ALWAYS"
        } else {
            "BY DEFAULT"
        };
        sql.push_str(&format!(" GENERATED {generation} AS IDENTITY"));
    } else if !column.default.trim().is_empty() {
        sql.push_str(" DEFAULT ");
        sql.push_str(column.default.trim());
    }
    if !column.nullable {
        sql.push_str(" NOT NULL");
    }
    sql
}

fn index_sql(schema: &str, table: &str, index: &IndexDef) -> String {
    let columns = index
        .columns
        .iter()
        .map(|column| {
            let mut sql = quote_identifier(&column.name);
            if column.descending {
                sql.push_str(" DESC");
            }
            sql
        })
        .collect::<Vec<_>>()
        .join(", ");
    // `CREATE [UNIQUE|BITMAP] INDEX` — Oracle's two kinds are exclusive, so a bitmap index takes
    // the keyword and nothing else; any other type (a function-based index, say) falls back to the
    // plain form rather than inventing syntax.
    let kind = if index.unique {
        "UNIQUE "
    } else if index.index_type.trim().eq_ignore_ascii_case("BITMAP") {
        "BITMAP "
    } else {
        ""
    };
    format!(
        "CREATE {kind}INDEX {} ON {} ({columns})",
        // Oracle creates an index in the *connected* user's schema, but an index belongs to its
        // table's schema (and `DROP INDEX` names it that way), so it is qualified here.
        qualify(schema, &index.name),
        qualify(schema, table)
    )
}

fn unique_key_clause(key: &UniqueKeyDef) -> String {
    let body = format!(
        "UNIQUE ({})",
        key.columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Vec<_>>()
            .join(", ")
    );
    if key.name.trim().is_empty() {
        body
    } else {
        format!("CONSTRAINT {} {body}", quote_identifier(&key.name))
    }
}

fn check_clause(check: &CheckDef) -> String {
    let body = format!("CHECK ({})", check.expression.trim());
    if check.name.trim().is_empty() {
        body
    } else {
        format!("CONSTRAINT {} {body}", quote_identifier(&check.name))
    }
}

fn foreign_key_clause(foreign_key: &ForeignKeyDef) -> String {
    let columns = foreign_key
        .columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    let referenced = foreign_key
        .referenced_columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    let referenced_table = if !foreign_key.referenced_schema.trim().is_empty() {
        qualify(
            foreign_key.referenced_schema.trim(),
            &foreign_key.referenced_table,
        )
    } else {
        match foreign_key.referenced_table.split_once('.') {
            // Compatibility with a schema loaded before `referenced_schema` existed.
            Some((schema, name)) => qualify(schema, name),
            None => quote_identifier(&foreign_key.referenced_table),
        }
    };
    let constraint = if foreign_key.name.trim().is_empty() {
        String::new()
    } else {
        format!("CONSTRAINT {} ", quote_identifier(&foreign_key.name))
    };
    let mut sql =
        format!("{constraint}FOREIGN KEY ({columns}) REFERENCES {referenced_table} ({referenced})");
    if !foreign_key.on_delete.trim().is_empty() {
        sql.push_str(" ON DELETE ");
        sql.push_str(foreign_key.on_delete.trim());
    }
    sql
}

/// Introspect an existing table into a [`TableSchema`].
pub(crate) async fn table_schema(
    connection: &OracleConnection,
    _database: &str,
    table: &str,
) -> Result<TableSchema> {
    let (schema, bare) = connection.resolve(table);
    connection
        .with_conn(move |raw| {
            let binds: Vec<String> = vec![schema.clone(), bare.clone()];
            let refs = crate::connection::bind_refs(&binds);

            let primary = crate::connection::primary_key_columns(raw, &schema, &bare)?;
            let column_cursor = raw
                .query(
                    "SELECT c.column_name, c.data_type, c.data_length, c.data_precision, \
                            c.data_scale, c.nullable, c.data_default, c.char_length, c.char_used, \
                            c.identity_column, cc.comments, ic.generation_type \
                     FROM all_tab_columns c \
                     LEFT JOIN all_col_comments cc \
                       ON cc.owner = c.owner AND cc.table_name = c.table_name \
                      AND cc.column_name = c.column_name \
                     LEFT JOIN all_tab_identity_cols ic \
                       ON ic.owner = c.owner AND ic.table_name = c.table_name \
                      AND ic.column_name = c.column_name \
                     WHERE c.owner = :1 AND c.table_name = :2 \
                     ORDER BY c.column_id",
                    &refs,
                )
                .map_err(map_query_error)?;
            let mut columns = Vec::new();
            for row in column_cursor {
                let row = row.map_err(map_query_error)?;
                let name = string_at(&row, 0);
                let data_type = base_type(&string_at(&row, 1));
                let data_length = number_at(&row, 2);
                let precision = number_at(&row, 3);
                let scale = number_at(&row, 4);
                let nullable = string_at(&row, 5).eq_ignore_ascii_case("Y");
                let default = string_at(&row, 6).trim().to_string();
                let char_length = number_at(&row, 7);
                let identity_column = string_at(&row, 9).eq_ignore_ascii_case("YES");
                let generation_type = string_at(&row, 11);
                let comment = string_at(&row, 10);
                let identity = if generation_type.is_empty() {
                    if identity_column {
                        "BY DEFAULT".to_string()
                    } else {
                        String::new()
                    }
                } else {
                    generation_type
                };

                let (length, decimals) = if data_type == "NUMBER" {
                    (
                        precision
                            .filter(|value| *value > 0)
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                        scale
                            .filter(|value| *value != 0)
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                    )
                } else if data_type == "DATE" || data_type.starts_with("TIMESTAMP") {
                    (String::new(), String::new())
                } else {
                    (
                        char_length
                            .or(data_length)
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                        String::new(),
                    )
                };
                columns.push(ColumnDef {
                    primary_key: primary.iter().any(|column| column == &name),
                    name,
                    data_type,
                    length,
                    decimals,
                    nullable,
                    default,
                    auto_increment: identity_column,
                    identity,
                    comment,
                    ..Default::default()
                });
            }

            let index_cursor = raw
                .query(
                    "SELECT i.index_name, i.uniqueness, i.index_type, c.column_name, c.descend, \
                            CASE WHEN EXISTS (SELECT 1 FROM all_constraints k \
                                              WHERE k.owner = i.table_owner \
                                                AND k.table_name = i.table_name \
                                                AND k.index_name = i.index_name \
                                                AND k.constraint_type = 'P') \
                                 THEN 1 ELSE 0 END AS is_primary \
                     FROM all_indexes i \
                     JOIN all_ind_columns c \
                       ON c.index_owner = i.owner AND c.index_name = i.index_name \
                     WHERE i.table_owner = :1 AND i.table_name = :2 \
                       AND NOT EXISTS (SELECT 1 FROM all_constraints u \
                                       WHERE u.owner = i.table_owner \
                                         AND u.table_name = i.table_name \
                                         AND u.index_name = i.index_name \
                                         AND u.constraint_type = 'U') \
                     ORDER BY i.index_name, c.column_position",
                    &refs,
                )
                .map_err(map_query_error)?;
            let mut indexes: Vec<IndexDef> = Vec::new();
            for row in index_cursor {
                let row = row.map_err(map_query_error)?;
                let name = string_at(&row, 0);
                let unique = string_at(&row, 1).eq_ignore_ascii_case("UNIQUE");
                let index_type = string_at(&row, 2);
                let primary = number_at(&row, 5).unwrap_or(0) == 1;
                // Oracle indexes have no prefix length (子部分 is MySQL's).
                let index_column = IndexColumnDef {
                    name: string_at(&row, 3),
                    descending: string_at(&row, 4).eq_ignore_ascii_case("DESC"),
                    prefix_length: String::new(),
                };
                match indexes.iter_mut().find(|index| index.name == name) {
                    Some(existing) => existing.columns.push(index_column),
                    None => indexes.push(IndexDef {
                        name,
                        columns: vec![index_column],
                        unique,
                        primary,
                        index_type,
                        comment: String::new(),
                    }),
                }
            }

            let fk_cursor = raw
                .query(
                    "SELECT c.constraint_name, cc.column_name, rc.owner, rc.table_name, \
                            rcc.column_name, c.delete_rule \
                     FROM all_constraints c \
                     JOIN all_cons_columns cc \
                       ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
                     JOIN all_constraints rc ON rc.constraint_name = c.r_constraint_name \
                     JOIN all_cons_columns rcc \
                       ON rcc.owner = rc.owner AND rcc.constraint_name = rc.constraint_name \
                      AND rcc.position = cc.position \
                     WHERE c.constraint_type = 'R' AND c.owner = :1 AND c.table_name = :2 \
                     ORDER BY c.constraint_name, cc.position",
                    &refs,
                )
                .map_err(map_query_error)?;
            let mut foreign_keys: Vec<ForeignKeyDef> = Vec::new();
            for row in fk_cursor {
                let row = row.map_err(map_query_error)?;
                let name = string_at(&row, 0);
                let column = string_at(&row, 1);
                let ref_schema = string_at(&row, 2);
                let ref_table = string_at(&row, 3);
                let ref_column = string_at(&row, 4);
                let delete_rule = string_at(&row, 5);
                match foreign_keys
                    .iter_mut()
                    .find(|foreign_key| foreign_key.name == name)
                {
                    Some(existing) => {
                        existing.columns.push(column);
                        existing.referenced_columns.push(ref_column);
                    }
                    None => foreign_keys.push(ForeignKeyDef {
                        name,
                        columns: vec![column],
                        referenced_schema: ref_schema,
                        referenced_table: ref_table,
                        referenced_columns: vec![ref_column],
                        on_delete: normalize_action(&delete_rule),
                        on_update: String::new(),
                    }),
                }
            }

            let unique_cursor = raw
                .query(
                    "SELECT c.constraint_name, cc.column_name \
                     FROM all_constraints c \
                     JOIN all_cons_columns cc \
                       ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name \
                     WHERE c.constraint_type = 'U' AND c.owner = :1 AND c.table_name = :2 \
                     ORDER BY c.constraint_name, cc.position",
                    &refs,
                )
                .map_err(map_query_error)?;
            let mut unique_keys: Vec<UniqueKeyDef> = Vec::new();
            for row in unique_cursor {
                let row = row.map_err(map_query_error)?;
                let name = string_at(&row, 0);
                let column = string_at(&row, 1);
                match unique_keys.iter_mut().find(|key| key.name == name) {
                    Some(existing) => existing.columns.push(column),
                    None => unique_keys.push(UniqueKeyDef {
                        name,
                        columns: vec![column],
                    }),
                }
            }

            // Oracle reports NOT NULL constraints as type `C` too. A system-named `SYS_C…`
            // constraint whose condition is `COL IS NOT NULL` is the implicit one, not a user
            // check; user checks (named or not) keep their expression. `SEARCH_CONDITION_VC` is
            // Oracle 12c+; on an older server the check query is skipped rather than failing the
            // whole designer load.
            let mut checks: Vec<CheckDef> = Vec::new();
            if let Ok(check_cursor) = raw.query(
                "SELECT c.constraint_name, c.search_condition_vc \
                 FROM all_constraints c \
                 WHERE c.constraint_type = 'C' AND c.owner = :1 AND c.table_name = :2 \
                   AND NOT (c.constraint_name LIKE 'SYS_C%' \
                            AND c.search_condition_vc LIKE '% IS NOT NULL') \
                 ORDER BY c.constraint_name",
                &refs,
            ) {
                for row in check_cursor {
                    let Ok(row) = row else { continue };
                    checks.push(CheckDef {
                        name: string_at(&row, 0),
                        expression: string_at(&row, 1).trim().to_string(),
                    });
                }
            }

            let trigger_cursor = raw
                .query(
                    "SELECT trigger_name FROM all_triggers \
                     WHERE table_owner = :1 AND table_name = :2 ORDER BY trigger_name",
                    &refs,
                )
                .map_err(map_query_error)?;
            let mut triggers = Vec::new();
            for row in trigger_cursor {
                let row = row.map_err(map_query_error)?;
                let name = string_at(&row, 0);
                let statement = crate::connection::fetch_ddl(raw, "TRIGGER", &name, &schema)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                triggers.push(TriggerDef {
                    name,
                    statement,
                    ..Default::default()
                });
            }

            let comment_cursor = raw
                .query(
                    "SELECT comments FROM all_tab_comments \
                     WHERE owner = :1 AND table_name = :2",
                    &refs,
                )
                .map_err(map_query_error)?;
            let mut comment = String::new();
            for row in comment_cursor {
                let row = row.map_err(map_query_error)?;
                comment = string_at(&row, 0);
            }

            Ok(TableSchema {
                columns,
                indexes,
                unique_keys,
                checks,
                excludes: Vec::new(),
                rules: Vec::new(),
                foreign_keys,
                triggers,
                partitioning: String::new(),
                options: TableOptions {
                    comment,
                    ..Default::default()
                },
            })
        })
        .await
}

/// Build the DDL that turns `original` into `modified` (or creates the table when `original` is
/// `None`).
/// `DROP`/`CREATE` statements for Oracle's complete trigger definitions.
pub(crate) fn trigger_sql(
    schema_name: &str,
    original: Option<&TableSchema>,
    modified: &TableSchema,
) -> Vec<String> {
    let mut statements = Vec::new();
    let original_triggers = original
        .map(|schema| schema.triggers.as_slice())
        .unwrap_or(&[]);
    for trigger in original_triggers {
        if trigger.name.trim().is_empty() {
            continue;
        }
        let unchanged = modified
            .triggers
            .iter()
            .find(|candidate| candidate.name == trigger.name)
            .is_some_and(|candidate| candidate == trigger);
        if !unchanged {
            statements.push(format!(
                "DROP TRIGGER {}",
                qualify(schema_name, &trigger.name)
            ));
        }
    }
    for trigger in &modified.triggers {
        // A new full definition has no name until the designer reloads; the statement carries it.
        if trigger.statement.trim().is_empty() {
            continue;
        }
        let unchanged = original
            .map(|schema| schema.triggers.as_slice())
            .is_some_and(|triggers| {
                triggers
                    .iter()
                    .find(|candidate| candidate.name == trigger.name)
                    .is_some_and(|candidate| candidate == trigger)
            });
        if !unchanged {
            statements.push(
                trigger
                    .statement
                    .trim()
                    .trim_end_matches('/')
                    .trim()
                    .trim_end_matches(';')
                    .trim_end()
                    .to_string(),
            );
        }
    }
    statements
}

pub(crate) fn table_schema_sql(
    schema: &str,
    table: &str,
    original: Option<&TableSchema>,
    modified: &TableSchema,
) -> String {
    let qualified = qualify(schema, table);
    let Some(original) = original else {
        return create_table_sql(schema, table, &qualified, modified);
    };

    let mut statements: Vec<String> = Vec::new();
    let original_columns: HashMap<&str, &ColumnDef> = original
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column))
        .collect();
    let modified_columns: HashMap<&str, &ColumnDef> = modified
        .columns
        .iter()
        .map(|column| (column.name.as_str(), column))
        .collect();
    let original_uniques: HashMap<&str, &UniqueKeyDef> = original
        .unique_keys
        .iter()
        .filter(|key| !key.name.trim().is_empty())
        .map(|key| (key.name.as_str(), key))
        .collect();
    let modified_uniques: HashMap<&str, &UniqueKeyDef> = modified
        .unique_keys
        .iter()
        .filter(|key| !key.name.trim().is_empty())
        .map(|key| (key.name.as_str(), key))
        .collect();
    let original_checks: HashMap<&str, &CheckDef> = original
        .checks
        .iter()
        .filter(|check| !check.name.trim().is_empty())
        .map(|check| (check.name.as_str(), check))
        .collect();
    let modified_checks: HashMap<&str, &CheckDef> = modified
        .checks
        .iter()
        .filter(|check| !check.name.trim().is_empty())
        .map(|check| (check.name.as_str(), check))
        .collect();

    // Foreign keys go first: a unique key they reference cannot be dropped before them. Then
    // unique/check constraints, before any column change: Oracle refuses to drop a column a
    // constraint still uses. New/changed ones are re-added after the columns exist.
    let original_fks: HashMap<&str, &ForeignKeyDef> = original
        .foreign_keys
        .iter()
        .filter(|foreign_key| !foreign_key.name.trim().is_empty())
        .map(|foreign_key| (foreign_key.name.as_str(), foreign_key))
        .collect();
    let modified_fks: HashMap<&str, &ForeignKeyDef> = modified
        .foreign_keys
        .iter()
        .filter(|foreign_key| !foreign_key.name.trim().is_empty())
        .map(|foreign_key| (foreign_key.name.as_str(), foreign_key))
        .collect();
    for foreign_key in &original.foreign_keys {
        if foreign_key.name.trim().is_empty() {
            continue;
        }
        let unchanged = modified_fks
            .get(foreign_key.name.as_str())
            .is_some_and(|candidate| *candidate == foreign_key);
        if !unchanged {
            statements.push(format!(
                "ALTER TABLE {qualified} DROP CONSTRAINT {}",
                quote_identifier(&foreign_key.name)
            ));
        }
    }

    for key in &original.unique_keys {
        if key.name.trim().is_empty() {
            continue;
        }
        let unchanged = modified_uniques
            .get(key.name.as_str())
            .is_some_and(|candidate| *candidate == key);
        if !unchanged {
            statements.push(format!(
                "ALTER TABLE {qualified} DROP CONSTRAINT {}",
                quote_identifier(&key.name)
            ));
        }
    }
    for check in &original.checks {
        if check.name.trim().is_empty() {
            continue;
        }
        let unchanged = modified_checks
            .get(check.name.as_str())
            .is_some_and(|candidate| *candidate == check);
        if !unchanged {
            statements.push(format!(
                "ALTER TABLE {qualified} DROP CONSTRAINT {}",
                quote_identifier(&check.name)
            ));
        }
    }

    let original_primary: Vec<&str> = original
        .columns
        .iter()
        .filter(|column| column.primary_key)
        .map(|column| column.name.as_str())
        .collect();
    let modified_primary: Vec<&str> = modified
        .columns
        .iter()
        .filter(|column| column.primary_key)
        .map(|column| column.name.as_str())
        .collect();
    if original_primary != modified_primary && !original_primary.is_empty() {
        statements.push(format!("ALTER TABLE {qualified} DROP PRIMARY KEY"));
    }

    let original_indexes: HashMap<&str, &IndexDef> = original
        .indexes
        .iter()
        .map(|index| (index.name.as_str(), index))
        .collect();
    let modified_indexes: HashMap<&str, &IndexDef> = modified
        .indexes
        .iter()
        .map(|index| (index.name.as_str(), index))
        .collect();
    for index in original.indexes.iter().filter(|index| !index.primary) {
        let unchanged = modified_indexes
            .get(index.name.as_str())
            .is_some_and(|candidate| *candidate == index);
        if !unchanged {
            statements.push(format!("DROP INDEX {}", qualify(schema, &index.name)));
        }
    }

    for column in &modified.columns {
        match original_columns.get(column.name.as_str()) {
            None => statements.push(format!(
                "ALTER TABLE {qualified} ADD ({})",
                column_sql(column)
            )),
            Some(previous) => {
                let shape_changed = previous.data_type != column.data_type
                    || previous.length != column.length
                    || previous.decimals != column.decimals
                    || previous.nullable != column.nullable;
                if shape_changed {
                    let mut modification = column_sql(column);
                    if column.nullable {
                        // `MODIFY (c type)` leaves nullability as-is; say it explicitly.
                        modification.push_str(" NULL");
                    }
                    statements.push(format!("ALTER TABLE {qualified} MODIFY ({modification})"));
                } else if previous.default != column.default && !column.auto_increment {
                    let clause = if column.default.trim().is_empty() {
                        "DEFAULT NULL".to_string()
                    } else {
                        format!("DEFAULT {}", column.default.trim())
                    };
                    statements.push(format!(
                        "ALTER TABLE {qualified} MODIFY ({} {clause})",
                        quote_identifier(&column.name)
                    ));
                }
                if previous.auto_increment != column.auto_increment {
                    let clause = if column.auto_increment {
                        if column.identity.trim().eq_ignore_ascii_case("ALWAYS") {
                            "GENERATED ALWAYS AS IDENTITY"
                        } else {
                            "GENERATED BY DEFAULT AS IDENTITY"
                        }
                    } else {
                        "DROP IDENTITY"
                    };
                    statements.push(format!(
                        "ALTER TABLE {qualified} MODIFY ({} {clause})",
                        quote_identifier(&column.name)
                    ));
                }
                if previous.comment != column.comment {
                    statements.push(comment_on_column(&qualified, &column.name, &column.comment));
                }
            }
        }
    }
    for column in &original.columns {
        if !modified_columns.contains_key(column.name.as_str()) {
            statements.push(format!(
                "ALTER TABLE {qualified} DROP COLUMN {}",
                quote_identifier(&column.name)
            ));
        }
    }

    if original_primary != modified_primary && !modified_primary.is_empty() {
        statements.push(format!(
            "ALTER TABLE {qualified} ADD PRIMARY KEY ({})",
            modified_primary
                .iter()
                .map(|column| quote_identifier(column))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    for index in modified.indexes.iter().filter(|index| !index.primary) {
        let unchanged = original_indexes
            .get(index.name.as_str())
            .is_some_and(|candidate| *candidate == index);
        if !unchanged {
            statements.push(index_sql(schema, table, index));
        }
    }

    for key in &modified.unique_keys {
        if key.name.trim().is_empty() || key.columns.is_empty() {
            continue;
        }
        let unchanged = original_uniques
            .get(key.name.as_str())
            .is_some_and(|candidate| *candidate == key);
        if !unchanged {
            statements.push(format!(
                "ALTER TABLE {qualified} ADD {}",
                unique_key_clause(key)
            ));
        }
    }
    for check in &modified.checks {
        if check.name.trim().is_empty() || check.expression.trim().is_empty() {
            continue;
        }
        let unchanged = original_checks
            .get(check.name.as_str())
            .is_some_and(|candidate| *candidate == check);
        if !unchanged {
            statements.push(format!(
                "ALTER TABLE {qualified} ADD {}",
                check_clause(check)
            ));
        }
    }

    for foreign_key in &modified.foreign_keys {
        if foreign_key.name.trim().is_empty() {
            continue;
        }
        let unchanged = original_fks
            .get(foreign_key.name.as_str())
            .is_some_and(|candidate| *candidate == foreign_key);
        if !unchanged {
            statements.push(format!(
                "ALTER TABLE {qualified} ADD {}",
                foreign_key_clause(foreign_key)
            ));
        }
    }

    if original.options.comment != modified.options.comment {
        statements.push(format!(
            "COMMENT ON TABLE {qualified} IS {}",
            if modified.options.comment.trim().is_empty() {
                "''".to_string()
            } else {
                quote_literal(modified.options.comment.trim())
            }
        ));
    }

    statements.join(";\n")
}

fn create_table_sql(
    schema: &str,
    table: &str,
    qualified: &str,
    schema_def: &TableSchema,
) -> String {
    let mut definitions: Vec<String> = schema_def.columns.iter().map(column_sql).collect();
    let primary: Vec<&ColumnDef> = schema_def
        .columns
        .iter()
        .filter(|column| column.primary_key)
        .collect();
    if !primary.is_empty() {
        definitions.push(format!(
            "PRIMARY KEY ({})",
            primary
                .iter()
                .map(|column| quote_identifier(&column.name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for key in &schema_def.unique_keys {
        if !key.columns.is_empty() {
            definitions.push(unique_key_clause(key));
        }
    }
    for check in &schema_def.checks {
        if !check.expression.trim().is_empty() {
            definitions.push(check_clause(check));
        }
    }
    for foreign_key in &schema_def.foreign_keys {
        definitions.push(foreign_key_clause(foreign_key));
    }
    let mut statements = vec![format!(
        "CREATE TABLE {qualified} (\n    {}\n)",
        definitions.join(",\n    ")
    )];
    for index in schema_def.indexes.iter().filter(|index| !index.primary) {
        statements.push(index_sql(schema, table, index));
    }
    if !schema_def.options.comment.trim().is_empty() {
        statements.push(format!(
            "COMMENT ON TABLE {qualified} IS {}",
            quote_literal(schema_def.options.comment.trim())
        ));
    }
    for column in &schema_def.columns {
        if !column.comment.trim().is_empty() {
            statements.push(comment_on_column(qualified, &column.name, &column.comment));
        }
    }
    statements.join(";\n")
}

fn comment_on_column(qualified: &str, column: &str, comment: &str) -> String {
    let literal = if comment.trim().is_empty() {
        "''".to_string()
    } else {
        quote_literal(comment.trim())
    };
    format!(
        "COMMENT ON COLUMN {qualified}.{} IS {literal}",
        quote_identifier(column)
    )
}

/// Drop a trailing `DEFAULT <expr>` clause from a rendered column definition.
fn normalize_action(rule: &str) -> String {
    match rule.to_ascii_uppercase().as_str() {
        "CASCADE" => "CASCADE".to_string(),
        "SET NULL" => "SET NULL".to_string(),
        _ => String::new(),
    }
}

fn string_at(row: &oracledb::Row, index: usize) -> String {
    row.get::<Option<String>>(index)
        .ok()
        .flatten()
        .unwrap_or_default()
}

fn number_at(row: &oracledb::Row, index: usize) -> Option<i64> {
    if let Ok(Some(value)) = row.get::<Option<oracledb::OracleNumber>>(index)
        && let Ok(value) = value.to_string().parse::<i64>()
    {
        return Some(value);
    }
    None
}

/// The stored trigger DDL of a table, for the backup and the designer's trigger list.
pub(crate) fn trigger_ddl(
    connection: &oracledb::Connection,
    schema: &str,
    table_name: &str,
) -> Result<Vec<String>> {
    let binds: Vec<String> = vec![schema.to_string(), table_name.to_string()];
    let refs = crate::connection::bind_refs(&binds);
    let cursor = connection
        .query(
            "SELECT trigger_name FROM all_triggers \
             WHERE table_owner = :1 AND table_name = :2 ORDER BY trigger_name",
            &refs,
        )
        .map_err(map_query_error)?;
    let mut names = Vec::new();
    for row in cursor {
        let row = row.map_err(map_query_error)?;
        if let Ok(Some(name)) = row.get::<Option<String>>(0) {
            names.push(name);
        }
    }
    let mut statements = Vec::new();
    for name in names {
        if let Some(ddl) = crate::connection::fetch_ddl(connection, "TRIGGER", &name, schema)? {
            statements.push(ddl);
        }
    }
    Ok(statements)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_create_table_with_identity_and_comments() {
        let schema = TableSchema {
            columns: vec![ColumnDef {
                name: "ID".to_string(),
                data_type: "NUMBER".to_string(),
                length: "10".to_string(),
                nullable: false,
                primary_key: true,
                auto_increment: true,
                ..Default::default()
            }],
            options: TableOptions {
                comment: "t".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let sql = table_schema_sql("APP", "T", None, &schema);
        assert!(sql.starts_with("CREATE TABLE \"APP\".\"T\""));
        assert!(sql.contains("\"ID\" NUMBER(10) GENERATED BY DEFAULT AS IDENTITY NOT NULL"));
        assert!(sql.contains("PRIMARY KEY (\"ID\")"));
        assert!(sql.contains("COMMENT ON TABLE \"APP\".\"T\" IS 't'"));
    }

    #[test]
    fn creates_a_table_mapped_from_mysql() {
        use rustgrid_core::{DriverDialect, TableSchema, map_schema};

        let source = TableSchema {
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: "bigint".to_string(),
                    nullable: false,
                    primary_key: true,
                    auto_increment: true,
                    ..Default::default()
                },
                ColumnDef {
                    name: "name".to_string(),
                    data_type: "varchar".to_string(),
                    length: "255".to_string(),
                    ..Default::default()
                },
                ColumnDef {
                    name: "payload".to_string(),
                    data_type: "longblob".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mapped = map_schema(&source, DriverDialect::Mysql, DriverDialect::Oracle);
        let sql = table_schema_sql("APP", "USERS", None, &mapped);
        assert!(sql.contains("\"id\" NUMBER(19)"), "{sql}");
        assert!(sql.contains("\"name\" VARCHAR2(255)"), "{sql}");
        assert!(sql.contains("\"payload\" BLOB"), "{sql}");
        assert!(sql.contains("GENERATED BY DEFAULT AS IDENTITY"), "{sql}");
    }

    #[test]
    fn preserves_generated_always_identity() {
        let schema = TableSchema {
            columns: vec![ColumnDef {
                name: "ID".to_string(),
                data_type: "NUMBER".to_string(),
                length: "10".to_string(),
                nullable: false,
                primary_key: true,
                auto_increment: true,
                identity: "ALWAYS".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let sql = table_schema_sql("APP", "T", None, &schema);
        assert!(
            sql.contains("\"ID\" NUMBER(10) GENERATED ALWAYS AS IDENTITY NOT NULL"),
            "{sql}"
        );
    }

    #[test]
    fn builds_full_trigger_diff() {
        let original = TableSchema {
            triggers: vec![TriggerDef {
                name: "TRG".to_string(),
                statement: "CREATE OR REPLACE TRIGGER \"APP\".\"TRG\" BEFORE INSERT ON \"APP\".\"T\" BEGIN NULL; END;".to_string(),
                ..TriggerDef::default()
            }],
            ..TableSchema::default()
        };
        let mut modified = original.clone();
        modified.triggers[0].statement =
            "CREATE OR REPLACE TRIGGER \"APP\".\"TRG\" AFTER INSERT ON \"APP\".\"T\" BEGIN NULL; END;"
                .to_string();

        let sql = trigger_sql("APP", Some(&original), &modified);
        assert_eq!(sql[0], "DROP TRIGGER \"APP\".\"TRG\"");
        assert!(sql[1].contains("AFTER INSERT"), "{sql:?}");
        assert!(!sql[1].ends_with(';'), "{sql:?}");
    }

    #[test]
    fn creates_and_reconciles_unique_and_check_constraints() {
        let mut schema = TableSchema {
            columns: vec![
                ColumnDef {
                    name: "ID".to_string(),
                    data_type: "NUMBER".to_string(),
                    ..Default::default()
                },
                ColumnDef {
                    name: "CODE".to_string(),
                    data_type: "VARCHAR2".to_string(),
                    length: "20".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        schema.unique_keys.push(UniqueKeyDef {
            name: "UQ_CODE".to_string(),
            columns: vec!["CODE".to_string()],
        });
        schema.checks.push(CheckDef {
            name: "CK_ID".to_string(),
            expression: "\"ID\">0".to_string(),
        });

        let sql = table_schema_sql("APP", "T", None, &schema);
        assert!(
            sql.contains("CONSTRAINT \"UQ_CODE\" UNIQUE (\"CODE\")"),
            "{sql}"
        );
        assert!(
            sql.contains("CONSTRAINT \"CK_ID\" CHECK (\"ID\">0)"),
            "{sql}"
        );

        let mut modified = schema.clone();
        modified.unique_keys[0].columns.push("ID".to_string());
        modified.checks[0].expression = "\"ID\">=0".to_string();
        let sql = table_schema_sql("APP", "T", Some(&schema), &modified);
        assert!(
            sql.contains("ALTER TABLE \"APP\".\"T\" DROP CONSTRAINT \"UQ_CODE\""),
            "{sql}"
        );
        assert!(
            sql.contains(
                "ALTER TABLE \"APP\".\"T\" ADD CONSTRAINT \"UQ_CODE\" UNIQUE (\"CODE\", \"ID\")"
            ),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER TABLE \"APP\".\"T\" ADD CONSTRAINT \"CK_ID\" CHECK (\"ID\">=0)"),
            "{sql}"
        );
    }

    #[test]
    fn alters_columns() {
        let original = TableSchema {
            columns: vec![ColumnDef {
                name: "A".to_string(),
                data_type: "NUMBER".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let modified = TableSchema {
            columns: vec![
                ColumnDef {
                    name: "A".to_string(),
                    data_type: "VARCHAR2".to_string(),
                    length: "20".to_string(),
                    ..Default::default()
                },
                ColumnDef {
                    name: "B".to_string(),
                    data_type: "DATE".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let sql = table_schema_sql("APP", "T", Some(&original), &modified);
        assert!(sql.contains("MODIFY (\"A\" VARCHAR2(20)"));
        assert!(sql.contains("ADD (\"B\" DATE"));
    }
}
