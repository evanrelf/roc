use can::expr::Expr;
use can::expr::Expr::*;
use can::symbol::Symbol;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub enum ReferenceCount {
    Unique,
    Shared,
}

impl ReferenceCount {
    pub fn add(_a: &ReferenceCount, _b: &ReferenceCount) -> Self {
        Self::Shared
    }

    pub fn or(a: &ReferenceCount, b: &ReferenceCount) -> Self {
        match (a, b) {
            (Self::Unique, Self::Unique) => Self::Unique,
            _ => Self::Shared,
        }
    }
}

fn register(symbol: &Symbol, usage: &mut HashMap<Symbol, ReferenceCount>) -> () {
    use self::ReferenceCount::*;
    let value = match usage.get(symbol) {
        None => Unique,
        Some(current) => ReferenceCount::add(current, &Unique),
    };

    usage.insert(symbol.clone(), value);
}

// NOTE (20 nov 2019) this could potentially be optimized:
//
// actually, I think it would also work to do it as HashMap<Symbol, Variable>
//
// you get the same "if there's already an entry in the map, then this must be shared"
// but also, at the same time, you can now retroactively mark that other Variable as Shared because you know what it is - you got it right there out of the map
// and if there is no entry for that Symbol in the map, then cool - you insert your current Variable and move on assuming uniqueness until someone else later decides (or not) that you were actually Shared

pub fn sharing_analysis(expr: &Expr, usage: &mut HashMap<Symbol, ReferenceCount>) -> () {
    match expr {
        Var(_, symbol) | FunctionPointer(_, symbol) => {
            register(symbol, usage);
        }

        List(_, elements) => {
            for element in elements {
                sharing_analysis(&element.value, usage);
            }
        }

        Case(_, boxed_loc_expr, branches) => {
            sharing_analysis(&boxed_loc_expr.value, usage);

            for (_pattern, branch) in branches {
                let mut local = usage.clone();

                sharing_analysis(&branch.value, &mut local);

                for (key, value) in local {
                    match usage.get(&key) {
                        None => {
                            usage.insert(key, value);
                        }
                        Some(current) => {
                            let result = ReferenceCount::or(current, &value);
                            usage.insert(key, result);
                        }
                    }
                }
            }
        }

        Defs(_, assignments, body) => {
            for (_pattern, value) in assignments {
                sharing_analysis(&value.value, usage);
            }

            sharing_analysis(&body.value, usage);
        }

        CallByName(symbol, arguments, _) => {
            register(symbol, usage);

            for argument in arguments {
                sharing_analysis(&argument.value, usage);
            }
        }

        CallPointer(function, arguments, _) => {
            sharing_analysis(function, usage);

            for argument in arguments {
                sharing_analysis(&argument.value, usage);
            }
        }

        Record(_, fields) => {
            for field in fields {
                sharing_analysis(&field.value.1.value, usage);
            }
        }

        Field(record, _) => {
            sharing_analysis(&record.value, usage);
        }
        Int(_) | Float(_) | Str(_) | BlockStr(_) | EmptyRecord | RuntimeError(_) => {}
    }
}
