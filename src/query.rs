// This file is part of MinSQL
// Copyright (c) 2019 MinIO, Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
use std::collections::HashMap;
use std::collections::HashSet;
use std::error;
use std::fmt;
use std::sync::{Arc, RwLock};

use futures::{stream, Future, Stream};
use hyper::{Body, Chunk, Request, Response};
use log::{error, info};
use regex::Regex;
use sqlparser::sqlast::SQLStatement;
use sqlparser::sqlparser::Parser;
use sqlparser::sqlparser::ParserError;

use bitflags::bitflags;
use lazy_static::lazy_static;

use crate::auth::Auth;
use crate::config::Config;
use crate::constants::SF_DATE;
use crate::constants::SF_EMAIL;
use crate::constants::SF_IP;
use crate::constants::SF_QUOTED;
use crate::constants::SF_URL;
use crate::dialect::MinSQLDialect;
use crate::filter::line_fails_query_conditions;
use crate::http::GenericError;
use crate::http::ResponseFuture;
use crate::http::{return_400, return_401};
use crate::storage::{list_msl_bucket_files, read_file_line_by_line};
use std::error::Error;

bitflags! {
    // ScanFlags determine which regex should be evaluated
    // If you are adding new values make sure to add the next power of 2 as
    // they are evaluated using a bitwise operation
    pub struct ScanFlags: u32 {
        const IP = 1;
        const EMAIL = 2;
        const DATE = 4;
        const QUOTED = 8;
        const URL = 16;
        const NONE = 32;
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PositionalColumn {
    position: i32,
    alias: String,
}

#[derive(Debug, Clone, PartialEq)]
struct SmartColumn {
    // $ip, $email...
    typed: String,
    // for $ip or $ip1 is 1, for $ip2 is 2 ...
    position: i32,
    // if this column was aliased
    alias: String,
}

#[derive(Debug)]
pub struct ParseSqlError;

impl fmt::Display for ParseSqlError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Error parsing sql")
    }
}

impl error::Error for ParseSqlError {
    fn description(&self) -> &str {
        "Error parsing sql"
    }

    fn cause(&self) -> Option<&error::Error> {
        // Generic error, underlying cause isn't tracked.
        None
    }
}

#[derive(Debug)]
pub enum QueryError {
    Underlying(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for QueryError {
    fn description(&self) -> &str {
        "query error?"
    }
}

pub struct Query {
    config: Arc<RwLock<Config>>,
}

impl Query {
    pub fn new(cfg: Arc<RwLock<Config>>) -> Query {
        Query { config: cfg }
    }

    pub fn parse_query(&self, entire_body: Chunk) -> Result<Vec<SQLStatement>, GenericError> {
        let payload: String = match String::from_utf8(entire_body.to_vec()) {
            Ok(str) => str,
            Err(err) => panic!("Couldn't convert buffer to string: {}", err),
        };

        // attempt to parse the payload
        let dialect = MinSQLDialect {};

        match Parser::parse_sql(&dialect, payload.clone()) {
            Ok(q) => Ok(q),
            Err(e) => {
                // Unable to parse query, match reason
                match e {
                    ParserError::TokenizerError(s) => {
                        error!("Failed to tokenize query `{}`: {}", payload.clone(), s);
                    }
                    ParserError::ParserError(s) => {
                        error!("Failed to parse query `{}`: {}", payload.clone(), s);
                    }
                }
                // TODO: Design a more informative error message
                Err(ParseSqlError.into())
            }
        }
    }

    pub fn validate_logs(&self, ast: &Vec<SQLStatement>) -> Option<GenericError> {
        let cfg = self.config.read().unwrap();
        // Validate all the tables for all the  queries, we don't want to start serving content
        // for the first query and then discover subsequent queries are invalid
        for query in ast {
            // find the table they want to query
            let some_table = match query {
                sqlparser::sqlast::SQLStatement::SQLQuery(q) => match q.body {
                    // TODO: Validate a single table
                    sqlparser::sqlast::SQLSetExpr::Select(ref bodyselect) => {
                        Some(bodyselect.from[0].relation.clone())
                    }
                    _ => None,
                },
                _ => {
                    error!("Not the type of query we support");
                    None
                }
            };
            if some_table == None {
                error!("No table found");
                return Some(ParseSqlError.into());
            }
            let table = some_table.unwrap().to_string();
            let loggy = cfg.get_log(&table);
            if loggy.is_none() {
                return Some(ParseSqlError.into());
            }
        }
        None
    }

    // performs a query on a log
    pub fn api_log_search(&self, req: Request<Body>, access_token: &String) -> ResponseFuture {
        let access_token = access_token.clone();
        let cfg = Arc::clone(&self.config);
        let query_c = Query::new(cfg);

        let query_state_holder = Arc::new(RwLock::new(StateHolder::new()));
        let query_state_holder = Arc::clone(&query_state_holder);
        // A web api to run against
        Box::new(
            req.into_body()
                .concat2() // Concatenate all chunks in the body
                .from_err()
                .and_then(move |entire_body| {
                    let ast = match query_c.parse_query(entire_body) {
                        Ok(v) => v,
                        Err(e) => {
                            return Ok(return_400(format!("{:?}", e).as_str()));
                        }
                    };
                    match query_c.validate_logs(&ast) {
                        None => (),
                        Some(_) => {
                            return Ok(return_400("invalid log name"));
                        }
                    };

                    // Translate the SQL AST into a `QueryParsing`
                    // that has all the elements needed to continue
                    let parsed_queries = match query_c.process_sql(&access_token, ast) {
                        Ok(v) => v,
                        Err(e) => {
                            return match e {
                                ProcessingQueryError::Fail(s) => Ok(return_400(s.clone().as_str())),
                                ProcessingQueryError::UnsupportedQuery(s) => {
                                    Ok(return_400(s.clone().as_str()))
                                }
                                ProcessingQueryError::NoTableFound(s) => {
                                    Ok(return_400(s.clone().as_str()))
                                }
                                ProcessingQueryError::Unauthorized(_s) => Ok(return_401()),
                            };
                        }
                    };
                    let total_querys = parsed_queries.len();
                    let mut writable_state = query_state_holder.write().unwrap();
                    writable_state.query_parsing = parsed_queries;

                    // prepare copies to go into the next future

                    let cfg = Arc::clone(&query_c.config);

                    let query_state_holder = Arc::clone(&query_state_holder);

                    let body_str = stream::iter_ok(0..total_querys)
                        .map(move |query_index| {
                            // for each query parse, read from all datasources for the log
                            let read_state_holder = query_state_holder.read().unwrap();
                            let q_parse = &read_state_holder.query_parsing[query_index].1;
                            let cfg_read = cfg.read().unwrap();
                            let log = cfg_read.get_log(&q_parse.log_name).unwrap();
                            let log_datastores = &log.datastores;

                            let limit = q_parse.limit.unwrap_or(std::u64::MAX);

                            let logs_ds_len = log_datastores.len();

                            // prepare copies to go into the next future
                            let cfg = Arc::clone(&cfg);
                            let query_state_holder = Arc::clone(&query_state_holder);

                            stream::iter_ok(0..logs_ds_len)
                                .map(move |log_ds_index| {
                                    let cfg_read = cfg.read().unwrap();
                                    let read_state_holder = query_state_holder.read().unwrap();

                                    let q_parse = &read_state_holder.query_parsing[query_index].1;
                                    let log = cfg_read.get_log(&q_parse.log_name).unwrap();

                                    let ds_name = &log.datastores[log_ds_index];

                                    let log_name = cfg
                                        .read()
                                        .unwrap()
                                        .get_log(&q_parse.log_name)
                                        .unwrap()
                                        .name
                                        .clone()
                                        .unwrap();
                                    // validation should make this unwrapping safe
                                    let ds = cfg_read.datastore.get(ds_name.as_str()).unwrap();

                                    let cfg2 = Arc::clone(&cfg);
                                    let query_state_holder2 = Arc::clone(&query_state_holder);

                                    // Returns Result<(ds, files), error>. Need to stop on error.
                                    // TODO: Stop on error
                                    list_msl_bucket_files(log_name.as_str(), &ds)
                                        .map(move |obj_key| {
                                            (query_index.clone(), log_ds_index.clone(), obj_key)
                                        })
                                        .map_err(|e| QueryError::Underlying(format!("{:?}", e))) //temporarely remove error, we need to adress this
                                        .map(move |(query_index, log_ds_index, obj_key)| {
                                            // TODO: Limit the number of results
                                            let read_state_holder =
                                                query_state_holder2.read().unwrap();
                                            let q_parse =
                                                &read_state_holder.query_parsing[query_index].1;

                                            let cfg_read = cfg2.read().unwrap();
                                            let log = cfg_read.get_log(&q_parse.log_name).unwrap();

                                            let ds_name = &log.datastores[log_ds_index];
                                            let ds = cfg_read.datastore.get(ds_name).unwrap();

                                            let query_state_holder2 =
                                                Arc::clone(&query_state_holder2);

                                            read_file_line_by_line(&obj_key, &ds)
                                                .filter_map(move |line| {
                                                    let read_state_holder =
                                                        query_state_holder2.read().unwrap();
                                                    let q = &read_state_holder.query_parsing
                                                        [query_index]
                                                        .0;
                                                    let q_parse = &read_state_holder.query_parsing
                                                        [query_index]
                                                        .1;

                                                    evaluate_query_on_line(&q, &q_parse, line)
                                                })
                                                .map_err(|e| {
                                                    QueryError::Underlying(format!("{:?}", e))
                                                }) //temporarely remove error, we need to adress this
                                        })
                                        .flatten()
                                })
                                .flatten()
                                .take(limit)
                        })
                        .flatten()
                        .map(|s| Chunk::from(s));

                    Ok(Response::new(Body::wrap_stream(body_str)))
                }),
        )
    }

    fn process_statement(
        &self,
        access_token: &String,
        query: SQLStatement,
    ) -> Result<(SQLStatement, QueryParsing), ProcessingQueryError> {
        lazy_static! {
            static ref SMART_FIELDS_RE: Regex =
                Regex::new(r"((\$(ip|email|date|url|quoted))([0-9]+)*)\b").unwrap();
        };

        // find the table they want to query
        let some_table = match query {
            sqlparser::sqlast::SQLStatement::SQLQuery(ref q) => {
                match q.body {
                    sqlparser::sqlast::SQLSetExpr::Select(ref bodyselect) => {
                        // TODO: Validate a single table
                        Some(bodyselect.from[0].relation.clone())
                    }
                    _ => {
                        return Err(ProcessingQueryError::Fail("No Table Found".to_string()));
                    }
                }
            }
            _ => {
                return Err(ProcessingQueryError::UnsupportedQuery(
                    "Unsupported query".to_string(),
                ));
            }
        };
        if some_table == None {
            return Err(ProcessingQueryError::NoTableFound(
                "No table was found in the query statement".to_string(),
            ));
        }
        let log_name = some_table.unwrap().to_string().clone();

        // check if we have access for the requested table
        let cfg = Arc::clone(&self.config);
        let auth_c = Auth::new(cfg);
        if !auth_c.token_has_access_to_log(&access_token[..], &log_name[..]) {
            return Err(ProcessingQueryError::Unauthorized(
                "Unauthorized".to_string(),
            ));
        }

        // determine our read strategy
        let read_all = match query {
            sqlparser::sqlast::SQLStatement::SQLQuery(ref q) => match q.body {
                sqlparser::sqlast::SQLSetExpr::Select(ref bodyselect) => {
                    let mut is_wildcard = false;
                    for projection in &bodyselect.projection {
                        if *projection == sqlparser::sqlast::SQLSelectItem::Wildcard {
                            is_wildcard = true
                        }
                    }
                    is_wildcard
                }
                _ => false,
            },
            _ => false,
        };

        let projections = match query {
            sqlparser::sqlast::SQLStatement::SQLQuery(ref q) => {
                match q.body {
                    sqlparser::sqlast::SQLSetExpr::Select(ref bodyselect) => {
                        bodyselect.projection.clone()
                    }
                    _ => {
                        Vec::new() //return empty
                    }
                }
            }
            _ => {
                Vec::new() //return empty
            }
        };

        let mut positional_fields: Vec<PositionalColumn> = Vec::new();
        let mut smart_fields: Vec<SmartColumn> = Vec::new();
        let mut smart_fields_set: HashSet<String> = HashSet::new();
        let mut projections_ordered: Vec<String> = Vec::new();
        // TODO: We should stream the data out as it becomes available to save memory
        for proj in &projections {
            match proj {
                sqlparser::sqlast::SQLSelectItem::UnnamedExpression(ref ast) => {
                    // we have an identifier
                    match ast {
                        sqlparser::sqlast::ASTNode::SQLIdentifier(ref identifier) => {
                            let id_name = &identifier[1..];
                            let position = match id_name.parse::<i32>() {
                                Ok(p) => p,
                                Err(_) => -1,
                            };
                            // if we were able to parse identifier as an i32 it's a positional
                            if position > 0 {
                                positional_fields.push(PositionalColumn {
                                    position: position,
                                    alias: identifier.clone(),
                                });
                                projections_ordered.push(identifier.clone());
                            } else {
                                // try to parse as as smart field
                                for sfield in SMART_FIELDS_RE.captures_iter(identifier) {
                                    let typed = sfield[2].to_string();
                                    let mut pos = 1;
                                    if sfield.get(4).is_none() == false {
                                        pos = match sfield[4].parse::<i32>() {
                                            Ok(p) => p,
                                            // technically this should never happen as the regex already validated an integer
                                            Err(_) => -1,
                                        };
                                    }
                                    // we use this set to keep track of active smart fields
                                    smart_fields_set.insert(typed.clone());
                                    // track the smartfield
                                    smart_fields.push(SmartColumn {
                                        typed: typed.clone(),
                                        position: pos,
                                        alias: identifier.clone(),
                                    });
                                    // record the order or extraction
                                    projections_ordered.push(identifier.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {} // for now let's not do anything on other Variances
            }
        }

        // see which fields in the conditions were not requested in the projections and extract them too
        let limit = match query {
            sqlparser::sqlast::SQLStatement::SQLQuery(ref q) => {
                match q.body {
                    sqlparser::sqlast::SQLSetExpr::Select(ref bodyselect) => {
                        for slct in &bodyselect.selection {
                            match slct {
                                sqlparser::sqlast::ASTNode::SQLIsNotNull(ast) => {
                                    let identifier = match **ast {
                                        sqlparser::sqlast::ASTNode::SQLIdentifier(
                                            ref identifier,
                                        ) => identifier.to_string(),
                                        _ => {
                                            // TODO: Should we be retunring anything at all?
                                            "".to_string()
                                        }
                                    };
                                    //positional or smart?
                                    let id_name = &identifier[1..];
                                    let position = match id_name.parse::<i32>() {
                                        Ok(p) => p,
                                        Err(_) => -1,
                                    };
                                    // if we were able to parse identifier as an i32 it's a positional
                                    if position > 0 {
                                        positional_fields.push(PositionalColumn {
                                            position: position,
                                            alias: identifier.clone(),
                                        });
                                    } else {
                                        // try to parse as as smart field
                                        for sfield in SMART_FIELDS_RE.captures_iter(&identifier[..])
                                        {
                                            let typed = sfield[2].to_string();
                                            let mut pos = 1;
                                            if sfield.get(4).is_none() == false {
                                                pos = match sfield[4].parse::<i32>() {
                                                    Ok(p) => p,
                                                    // technically this should never happen as the regex already validated an integer
                                                    Err(_) => -1,
                                                };
                                            }
                                            // we use this set to keep track of active smart fields
                                            smart_fields_set.insert(typed.clone());
                                            // track the smartfield
                                            smart_fields.push(SmartColumn {
                                                typed: typed.clone(),
                                                position: pos,
                                                alias: identifier.clone(),
                                            });
                                        }
                                    }
                                }
                                sqlparser::sqlast::ASTNode::SQLIsNull(ast) => {
                                    let identifier = match **ast {
                                        sqlparser::sqlast::ASTNode::SQLIdentifier(
                                            ref identifier,
                                        ) => identifier.to_string(),
                                        _ => {
                                            // TODO: Should we be retunring anything at all?
                                            "".to_string()
                                        }
                                    };
                                    //positional or smart?
                                    let id_name = &identifier[1..];
                                    let position = match id_name.parse::<i32>() {
                                        Ok(p) => p,
                                        Err(_) => -1,
                                    };
                                    // if we were able to parse identifier as an i32 it's a positional
                                    if position > 0 {
                                        positional_fields.push(PositionalColumn {
                                            position: position,
                                            alias: identifier.clone(),
                                        });
                                    } else {
                                        // try to parse as as smart field
                                        for sfield in SMART_FIELDS_RE.captures_iter(&identifier[..])
                                        {
                                            let typed = sfield[2].to_string();
                                            let mut pos = 1;
                                            if sfield.get(4).is_none() == false {
                                                pos = match sfield[4].parse::<i32>() {
                                                    Ok(p) => p,
                                                    // technically this should never happen as the regex already validated an integer
                                                    Err(_) => -1,
                                                };
                                            }
                                            // we use this set to keep track of active smart fields
                                            smart_fields_set.insert(typed.clone());
                                            // track the smartfield
                                            smart_fields.push(SmartColumn {
                                                typed: typed.clone(),
                                                position: pos,
                                                alias: identifier.clone(),
                                            });
                                        }
                                    }
                                }
                                sqlparser::sqlast::ASTNode::SQLBinaryOp {
                                    left,
                                    op: _,
                                    right: _,
                                } => {
                                    let identifier = left.to_string();

                                    //positional or smart?
                                    let id_name = &identifier[1..];
                                    let position = match id_name.parse::<i32>() {
                                        Ok(p) => p,
                                        Err(_) => -1,
                                    };
                                    // if we were able to parse identifier as an i32 it's a positional
                                    if position > 0 {
                                        positional_fields.push(PositionalColumn {
                                            position: position,
                                            alias: identifier.clone(),
                                        });
                                    } else {
                                        // try to parse as as smart field
                                        for sfield in SMART_FIELDS_RE.captures_iter(&identifier[..])
                                        {
                                            let typed = sfield[2].to_string();
                                            let mut pos = 1;
                                            if sfield.get(4).is_none() == false {
                                                pos = match sfield[4].parse::<i32>() {
                                                    Ok(p) => p,
                                                    // technically this should never happen as the regex already validated an integer
                                                    Err(_) => -1,
                                                };
                                            }
                                            // we use this set to keep track of active smart fields
                                            smart_fields_set.insert(typed.clone());
                                            // track the smartfield
                                            smart_fields.push(SmartColumn {
                                                typed: typed.clone(),
                                                position: pos,
                                                alias: identifier.clone(),
                                            });
                                        }
                                    }
                                }
                                _ => {
                                    info!("Unhandled operation");
                                }
                            }
                        }
                    }
                    _ => {}
                }
                match &q.limit {
                    Some(limit_node) => match limit_node {
                        sqlparser::sqlast::ASTNode::SQLValue(val) => match val {
                            sqlparser::sqlast::Value::Long(l) => Some(l.clone()),
                            _ => None,
                        },
                        _ => None,
                    },
                    None => None,
                }
            }
            _ => None,
        };

        // Build the parsing flags used by scanlog
        let mut scan_flags: ScanFlags = ScanFlags::NONE;
        for sfield_type in smart_fields_set {
            let flag = match sfield_type.as_ref() {
                "$ip" => ScanFlags::IP,
                "$email" => ScanFlags::EMAIL,
                "$date" => ScanFlags::DATE,
                "$quoted" => ScanFlags::QUOTED,
                "$url" => ScanFlags::URL,
                _ => ScanFlags::NONE,
            };
            if scan_flags == ScanFlags::NONE {
                scan_flags = flag;
            } else {
                scan_flags = scan_flags | flag;
            }
        }

        // we keep track of the parsing of the queries via their signature.
        Ok((
            query,
            QueryParsing {
                log_name,
                read_all,
                scan_flags,
                positional_fields,
                smart_fields,
                projections_ordered,
                limit,
            },
        ))
    }

    /// Parses a vector sql statements and returns a parsed summary
    /// structure for each.
    fn process_sql(
        &self,
        access_token: &String,
        ast: Vec<SQLStatement>,
    ) -> Result<Vec<(SQLStatement, QueryParsing)>, ProcessingQueryError> {
        ast.into_iter()
            .map(|q| self.process_statement(&access_token, q))
            .collect()
    }
}

pub fn scanlog(text: &String, flags: ScanFlags) -> HashMap<String, Vec<String>> {
    // Compile the regex only once
    lazy_static! {
        static ref IP_RE :Regex= Regex::new(r"(((25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9][0-9]|[0-9])\.){3}(25[0-5]|2[0-4][0-9]|1[0-9]{2}|[1-9][0-9]|[0-9]))").unwrap();
        static ref EMAIL_RE :Regex= Regex::new(r"([\w\.!#$%&'*+\-=?\^_`{|}~]+@([\w\d-]+\.)+[\w]{2,4})").unwrap();
        // TODO: This regex matches a fairly simple date format, improve : 2019-05-23
        static ref DATE_RE :Regex= Regex::new(r"((19[789]\d|2\d{3})[-/](0[1-9]|1[1-2])[-/](0[1-9]|[1-2][0-9]|3[0-1]*))|((0[1-9]|[1-2][0-9]|3[0-1]*)[-/](Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec|(0[1-9]|1[1-2]))[-/](19[789]\d|2\d{3}))").unwrap();
        static ref QUOTED_RE :Regex= Regex::new("((\"(.*?)\")|'(.*?)')").unwrap();
        static ref URL_RE :Regex= Regex::new(r#"(https?|ftp)://[^\s/$.?#].[^()\]\[\s]*"#).unwrap();
    }
    let mut results: HashMap<String, Vec<String>> = HashMap::new();

    if flags.contains(ScanFlags::IP) {
        let mut items: Vec<String> = Vec::new();
        for cap in IP_RE.captures_iter(text) {
            items.push(cap[0].to_string())
        }
        results.insert(SF_IP.to_string(), items);
    }
    if flags.contains(ScanFlags::EMAIL) {
        let mut items: Vec<String> = Vec::new();
        for cap in EMAIL_RE.captures_iter(text) {
            items.push(cap[0].to_string())
        }
        results.insert(SF_EMAIL.to_string(), items);
    }
    if flags.contains(ScanFlags::DATE) {
        let mut items: Vec<String> = Vec::new();
        for cap in DATE_RE.captures_iter(text) {
            items.push(cap[0].to_string())
        }
        results.insert(SF_DATE.to_string(), items);
    }
    if flags.contains(ScanFlags::QUOTED) {
        let mut items: Vec<String> = Vec::new();
        for cap in QUOTED_RE.captures_iter(text) {
            items.push(cap[0].to_string())
        }
        results.insert(SF_QUOTED.to_string(), items);
    }
    if flags.contains(ScanFlags::URL) {
        let mut items: Vec<String> = Vec::new();
        for cap in URL_RE.captures_iter(text) {
            items.push(cap[0].to_string())
        }
        results.insert(SF_URL.to_string(), items);
    }
    results
}

fn extract_positional_fields(
    projection_values: &mut HashMap<String, String>,
    query_data: &QueryParsing,
    line: String,
) {
    if query_data.positional_fields.len() > 0 {
        // TODO: Use separator construct from header
        let parts: Vec<&str> = line.split(" ").collect();
        for pos in &query_data.positional_fields {
            let key = pos.alias.clone();
            if pos.position - 1 < (parts.len() as i32) {
                projection_values.insert(key, parts[(pos.position - 1) as usize].to_string());
            } else {
                projection_values.insert(key, "".to_string());
            }
        }
    }
}

fn extract_smart_fields(
    projection_values: &mut HashMap<String, String>,
    query_data: &QueryParsing,
    line: String,
) {
    if query_data.smart_fields.len() > 0 {
        let found_vals = scanlog(&line.to_string(), query_data.scan_flags);
        for smt in &query_data.smart_fields {
            if found_vals.contains_key(&smt.typed[..]) {
                // if the requested position is available
                let key = smt.alias.clone();
                if smt.position - 1 < (found_vals[&smt.typed].len() as i32) {
                    projection_values.insert(
                        key,
                        found_vals[&smt.typed][(smt.position - 1) as usize].clone(),
                    );
                } else {
                    projection_values.insert(key, "".to_string());
                }
            }
        }
    }
}

fn mk_output_line(
    projection_values: &HashMap<String, String>,
    query_data: &QueryParsing,
    line: String,
) -> String {
    if query_data.read_all {
        line + &"\n"
    } else {
        // build the result iterate over the ordered resulting
        // projections
        let field_values: Vec<String> = query_data
            .projections_ordered
            .iter()
            .map(|x| x.clone())
            .filter(|proj| {
                // check if it's in positionals
                projection_values.contains_key(proj)
            })
            .map(|proj| projection_values[&proj].clone())
            .collect();

        // TODO: When adding CSV output, change the separator
        field_values.join(" ") + &"\n"
    }
}

fn evaluate_query_on_line(
    query: &SQLStatement,
    query_data: &QueryParsing,
    line: String,
) -> Option<String> {
    let mut projection_values: HashMap<String, String> = HashMap::new();

    extract_positional_fields(&mut projection_values, query_data, line.clone());

    extract_smart_fields(&mut projection_values, query_data, line.clone());

    // filter the line
    let skip_line = line_fails_query_conditions(&line, &query, &projection_values);

    if !skip_line {
        Some(mk_output_line(&projection_values, query_data, line.clone()))
    } else {
        None
    }
}

/// This struct represents the reading and filtering parameters that MinSQL uses to filter and
/// format the returned data.
#[derive(Debug, Clone)]
struct QueryParsing {
    log_name: String,
    read_all: bool,
    scan_flags: ScanFlags,
    positional_fields: Vec<PositionalColumn>,
    smart_fields: Vec<SmartColumn>,
    projections_ordered: Vec<String>,
    limit: Option<u64>,
}

enum ProcessingQueryError {
    Fail(String),
    UnsupportedQuery(String),
    NoTableFound(String),
    Unauthorized(String),
}

struct StateHolder {
    query_parsing: Vec<(SQLStatement, QueryParsing)>,
}

impl StateHolder {
    fn new() -> StateHolder {
        StateHolder {
            query_parsing: Vec::new(),
        }
    }
}

#[cfg(test)]
mod query_tests {
    use crate::config::{Config, Log, LogAuth};

    use super::*;

    // Generates a Config object with only one auth item for one log
    fn get_ds_log_auth_config_for(log_name: String, token: &String) -> Config {
        let mut log_map = HashMap::new();
        log_map.insert(
            log_name.clone(),
            Log {
                name: Some(log_name.clone()),
                datastores: Vec::new(),
                commit_window: "5s".to_string(),
            },
        );

        let mut log_auth_map: HashMap<String, LogAuth> = HashMap::new();
        log_auth_map.insert(
            log_name,
            LogAuth {
                token: token.clone(),
                api: Vec::new(),
                expire: "".to_string(),
                status: "".to_string(),
            },
        );

        let mut auth = HashMap::new();
        auth.insert(token.clone(), log_auth_map);

        let cfg = Config {
            version: "1".to_string(),
            server: None,
            datastore: HashMap::new(),
            log: log_map,
            auth: auth,
        };
        cfg
    }

    #[test]
    fn process_simple_select() {
        let access_token = "TOKEN1".to_string();

        let cfg = get_ds_log_auth_config_for("mylog".to_string(), &access_token);
        let cfg = Arc::new(RwLock::new(cfg));
        let query_c = Query::new(cfg);

        let query = "SELECT * FROM mylog".to_string();
        let ast = query_c.parse_query(Chunk::from(query.clone())).unwrap();
        let queries_parse = query_c.process_sql(&access_token, ast);

        match queries_parse {
            Ok(pq) => {
                let mqp = &pq[0].1;
                assert_eq!(mqp.log_name, "mylog");
                assert_eq!(mqp.read_all, true);
            }
            _ => panic!("error"),
        }
    }

    #[test]
    fn process_simple_select_limit() {
        let access_token = "TOKEN1".to_string();

        let cfg = get_ds_log_auth_config_for("mylog".to_string(), &access_token);
        let cfg = Arc::new(RwLock::new(cfg));
        let query_c = Query::new(cfg);

        let query = "SELECT * FROM mylog LIMIT 10".to_string();
        let ast = query_c.parse_query(Chunk::from(query.clone())).unwrap();
        let queries_parse = query_c.process_sql(&access_token, ast);

        match queries_parse {
            Ok(pq) => {
                let mqp = &pq[0].1;
                assert_eq!(mqp.log_name, "mylog");
                assert_eq!(mqp.read_all, true);
                match mqp.limit {
                    Some(l) => assert_eq!(l, 10),
                    None => panic!("NO LIMIT FOUND"),
                }
            }
            _ => panic!("error"),
        }
    }

    #[test]
    fn process_positional_fields_select() {
        let access_token = "TOKEN1".to_string();

        let cfg = get_ds_log_auth_config_for("mylog".to_string(), &access_token);
        let cfg = Arc::new(RwLock::new(cfg));
        let query_c = Query::new(cfg);

        let query = "SELECT $1, $4 FROM mylog".to_string();
        let ast = query_c.parse_query(Chunk::from(query.clone())).unwrap();
        let queries_parse = query_c.process_sql(&access_token, ast);

        match queries_parse {
            Ok(pq) => {
                let mqp = &pq[0].1;
                assert_eq!(mqp.log_name, "mylog");
                assert_eq!(
                    mqp.positional_fields,
                    vec![
                        PositionalColumn {
                            position: 1,
                            alias: "$1".to_string(),
                        },
                        PositionalColumn {
                            position: 4,
                            alias: "$4".to_string(),
                        }
                    ]
                )
            }
            _ => panic!("error"),
        }
    }

    #[test]
    fn process_positional_fields_select_limit() {
        let access_token = "TOKEN1".to_string();

        let cfg = get_ds_log_auth_config_for("mylog".to_string(), &access_token);
        let cfg = Arc::new(RwLock::new(cfg));
        let query_c = Query::new(cfg);

        let query = "SELECT $1, $4 FROM mylog LIMIT 10".to_string();
        let ast = query_c.parse_query(Chunk::from(query.clone())).unwrap();
        let queries_parse = query_c.process_sql(&access_token, ast);

        match queries_parse {
            Ok(pq) => {
                let mqp = &pq[0].1;
                assert_eq!(mqp.log_name, "mylog");
                assert_eq!(
                    mqp.positional_fields,
                    vec![
                        PositionalColumn {
                            position: 1,
                            alias: "$1".to_string(),
                        },
                        PositionalColumn {
                            position: 4,
                            alias: "$4".to_string(),
                        }
                    ]
                );
                assert_eq!(
                    mqp.projections_ordered,
                    vec!["$1".to_string(), "$4".to_string()],
                    "Order of fields is incorrect"
                );
                match mqp.limit {
                    Some(l) => assert_eq!(l, 10),
                    None => panic!("NO LIMIT FOUND"),
                }
            }
            _ => panic!("error"),
        }
    }

    #[test]
    fn process_smart_fields_select_limit() {
        let access_token = "TOKEN1".to_string();

        let cfg = get_ds_log_auth_config_for("mylog".to_string(), &access_token);
        let cfg = Arc::new(RwLock::new(cfg));
        let query_c = Query::new(cfg);

        let query = "SELECT $ip, $email FROM mylog LIMIT 10".to_string();
        let ast = query_c.parse_query(Chunk::from(query.clone())).unwrap();
        let queries_parse = query_c.process_sql(&access_token, ast);

        match queries_parse {
            Ok(pq) => {
                let mqp = &pq[0].1;
                assert_eq!(mqp.log_name, "mylog");
                assert_eq!(
                    mqp.smart_fields,
                    vec![
                        SmartColumn {
                            typed: "$ip".to_string(),
                            position: 1,
                            alias: "$ip".to_string(),
                        },
                        SmartColumn {
                            typed: "$email".to_string(),
                            position: 1,
                            alias: "$email".to_string(),
                        }
                    ]
                );
                assert_eq!(
                    mqp.projections_ordered,
                    vec!["$ip".to_string(), "$email".to_string()],
                    "Order of fields is incorrect"
                );
                assert_eq!(
                    mqp.scan_flags,
                    ScanFlags::IP | ScanFlags::EMAIL,
                    "Scan flags don't match"
                );
                match mqp.limit {
                    Some(l) => assert_eq!(l, 10),
                    None => panic!("NO LIMIT FOUND"),
                }
            }
            _ => panic!("error"),
        }
    }

    #[test]
    fn process_mixed_smart_positional_fields_select_limit() {
        let access_token = "TOKEN1".to_string();

        let cfg = get_ds_log_auth_config_for("mylog".to_string(), &access_token);
        let cfg = Arc::new(RwLock::new(cfg));
        let query_c = Query::new(cfg);

        let query = "SELECT $2, $ip, $email FROM mylog LIMIT 10".to_string();
        let ast = query_c.parse_query(Chunk::from(query.clone())).unwrap();
        let queries_parse = query_c.process_sql(&access_token, ast);

        match queries_parse {
            Ok(pq) => {
                let mqp = &pq[0].1;
                assert_eq!(mqp.log_name, "mylog");
                assert_eq!(
                    mqp.smart_fields,
                    vec![
                        SmartColumn {
                            typed: "$ip".to_string(),
                            position: 1,
                            alias: "$ip".to_string(),
                        },
                        SmartColumn {
                            typed: "$email".to_string(),
                            position: 1,
                            alias: "$email".to_string(),
                        }
                    ]
                );
                assert_eq!(
                    mqp.positional_fields,
                    vec![PositionalColumn {
                        position: 2,
                        alias: "$2".to_string(),
                    }]
                );
                assert_eq!(
                    mqp.projections_ordered,
                    vec!["$2".to_string(), "$ip".to_string(), "$email".to_string()],
                    "Order of fields is incorrect"
                );
                match mqp.limit {
                    Some(l) => assert_eq!(l, 10),
                    None => panic!("NO LIMIT FOUND"),
                }
            }
            _ => panic!("error parsing query"),
        }
    }

    #[test]
    #[should_panic]
    fn process_invalid_query() {
        let access_token = "TOKEN1".to_string();

        let cfg = get_ds_log_auth_config_for("mylog".to_string(), &access_token);
        let cfg = Arc::new(RwLock::new(cfg));
        let query_c = Query::new(cfg);

        let query = "INSERT INTO mylog ($line) VALES ('line')".to_string();
        match query_c.parse_query(Chunk::from(query.clone())) {
            Ok(_) => (),
            Err(_) => {
                panic!("Expected invalid query");
            }
        }
    }

    #[test]
    fn process_simple_select_invalid_access() {
        let provided_access_token = "TOKEN2".to_string();
        let access_token = "TOKEN1".to_string();

        let cfg = get_ds_log_auth_config_for("mylog".to_string(), &access_token);
        let cfg = Arc::new(RwLock::new(cfg));
        let query_c = Query::new(cfg);

        let query = "SELECT * FROM mylog".to_string();
        let ast = query_c.parse_query(Chunk::from(query.clone())).unwrap();
        let queries_parse = query_c.process_sql(&provided_access_token, ast);

        match queries_parse {
            Ok(pq) => {
                let mqp = &pq[0].1;
                assert_eq!(mqp.log_name, "mylog");
                assert_eq!(mqp.read_all, true);
            }
            Err(e) => match e {
                ProcessingQueryError::Unauthorized(_) => assert!(true),
                _ => panic!("Incorrect error"),
            },
        }
    }

    #[test]
    fn process_simple_select_invalid_table() {
        let provided_access_token = "TOKEN2".to_string();
        let access_token = "TOKEN1".to_string();

        let cfg = get_ds_log_auth_config_for("mylog".to_string(), &access_token);
        let cfg = Arc::new(RwLock::new(cfg));
        let query_c = Query::new(cfg);

        let query = "SELECT * FROM incorrect_log".to_string();
        let ast = query_c.parse_query(Chunk::from(query.clone())).unwrap();
        let queries_parse = query_c.process_sql(&provided_access_token, ast);

        match queries_parse {
            Ok(pq) => {
                let mqp = &pq[0].1;
                assert_eq!(mqp.log_name, "mylog");
                assert_eq!(mqp.read_all, true);
            }
            Err(e) => match e {
                ProcessingQueryError::Unauthorized(_) => assert!(true),
                _ => panic!("Incorrect error"),
            },
        }
    }

    #[test]
    fn validate_invalid_table() {
        let access_token = "TOKEN1".to_string();

        let cfg = get_ds_log_auth_config_for("mylog".to_string(), &access_token);
        let cfg = Arc::new(RwLock::new(cfg));
        let query_c = Query::new(cfg);

        let query = "SELECT * FROM incorrect_log".to_string();
        let ast = query_c.parse_query(Chunk::from(query.clone())).unwrap();
        match query_c.validate_logs(&ast) {
            None => panic!("Should have reported an error"),
            Some(_) => assert!(true),
        }
    }
}
