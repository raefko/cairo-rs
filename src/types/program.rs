use crate::serde::deserialize_program::{
    deserialize_program, deserialize_program_from_string, Attribute, HintParams, Identifier,
    ReferenceManager,
};
use crate::types::errors::program_errors::ProgramError;
use crate::types::relocatable::MaybeRelocatable;
use num_bigint::{BigInt, Sign};
use std::fs::File;
use std::io::{BufReader, Read};
use std::{collections::HashMap, path::Path};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub builtins: Vec<String>,
    pub prime: BigInt,
    pub data: Vec<MaybeRelocatable>,
    pub constants: HashMap<String, BigInt>,
    pub main: Option<usize>,
    //start and end labels will only be used in proof-mode
    pub start: Option<usize>,
    pub end: Option<usize>,
    pub hints: HashMap<usize, Vec<HintParams>>,
    pub reference_manager: ReferenceManager,
    pub identifiers: HashMap<String, Identifier>,
    pub error_message_attributes: Vec<Attribute>,
}

impl Program {
    pub fn from_file(path: &Path, entrypoint: Option<&str>) -> Result<Program, ProgramError> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);

        deserialize_program(reader, entrypoint)
    }

    pub fn from_string(json: &String, entrypoint: Option<&str>) -> Result<Program, ProgramError> {
        deserialize_program_from_string(json, entrypoint)
    }

    pub fn from_reader(
        reader: impl Read,
        entrypoint: Option<&str>,
    ) -> Result<Program, ProgramError> {
        deserialize_program(reader, entrypoint)
    }
}

impl Default for Program {
    fn default() -> Self {
        Program {
            builtins: Vec::new(),
            prime: BigInt::new(Sign::Plus, vec![1, 0, 0, 0, 0, 0, 17, 134217728]),
            data: Vec::new(),
            constants: HashMap::new(),
            main: None,
            start: None,
            end: None,
            hints: HashMap::new(),
            reference_manager: ReferenceManager {
                references: Vec::new(),
            },
            identifiers: HashMap::new(),
            error_message_attributes: Vec::new(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bigint, bigint_str};
    use num_traits::FromPrimitive;

    #[test]

    fn deserialize_program_from_str_test() {
        let json_artifact = r#"
        {
            "attributes": [
                {
                    "accessible_scopes": [
                        "openzeppelin.security.safemath.library",
                        "openzeppelin.security.safemath.library.SafeUint256",
                        "openzeppelin.security.safemath.library.SafeUint256.add"
                    ],
                    "end_pc": 381,
                    "flow_tracking_data": {
                        "ap_tracking": {
                            "group": 14,
                            "offset": 35
                        },
                        "reference_ids": {}
                    },
                    "name": "error_message",
                    "start_pc": 379,
                    "value": "SafeUint256: addition overflow"
                },
                {
                    "accessible_scopes": [
                        "openzeppelin.security.safemath.library",
                        "openzeppelin.security.safemath.library.SafeUint256",
                        "openzeppelin.security.safemath.library.SafeUint256.sub_le"
                    ],
                    "end_pc": 404,
                    "flow_tracking_data": {
                        "ap_tracking": {
                            "group": 15,
                            "offset": 60
                        },
                        "reference_ids": {}
                    },
                    "name": "unknow",
                    "start_pc": 402,
                    "value": "unknow"
                }
            ],
            "builtins": [],
            "data": [
                "0x480680017fff8000",
                "0x3e8",
                "0x480680017fff8000",
                "0x7d0",
                "0x48307fff7ffe8000",
                "0x208b7fff7fff7ffe"
            ],
            "debug_info": {
                "file_contents": {},
                "instruction_locations": {
                    "0": {
                        "accessible_scopes": [
                            "__main__",
                            "__main__.main"
                        ],
                        "flow_tracking_data": {
                            "ap_tracking": {
                                "group": 0,
                                "offset": 0
                            },
                            "reference_ids": {}
                        },
                        "hints": [],
                        "inst": {
                            "end_col": 22,
                            "end_line": 2,
                            "input_file": {
                                "filename": "test.cairo"
                            },
                            "start_col": 5,
                            "start_line": 2
                        }
                    },
                    "2": {
                        "accessible_scopes": [
                            "__main__",
                            "__main__.main"
                        ],
                        "flow_tracking_data": {
                            "ap_tracking": {
                                "group": 0,
                                "offset": 1
                            },
                            "reference_ids": {}
                        },
                        "hints": [],
                        "inst": {
                            "end_col": 22,
                            "end_line": 3,
                            "input_file": {
                                "filename": "test.cairo"
                            },
                            "start_col": 5,
                            "start_line": 3
                        }
                    },
                    "4": {
                        "accessible_scopes": [
                            "__main__",
                            "__main__.main"
                        ],
                        "flow_tracking_data": {
                            "ap_tracking": {
                                "group": 0,
                                "offset": 2
                            },
                            "reference_ids": {}
                        },
                        "hints": [],
                        "inst": {
                            "end_col": 37,
                            "end_line": 4,
                            "input_file": {
                                "filename": "test.cairo"
                            },
                            "start_col": 5,
                            "start_line": 4
                        }
                    },
                    "5": {
                        "accessible_scopes": [
                            "__main__",
                            "__main__.main"
                        ],
                        "flow_tracking_data": {
                            "ap_tracking": {
                                "group": 0,
                                "offset": 3
                            },
                            "reference_ids": {}
                        },
                        "hints": [],
                        "inst": {
                            "end_col": 8,
                            "end_line": 5,
                            "input_file": {
                                "filename": "test.cairo"
                            },
                            "start_col": 5,
                            "start_line": 5
                        }
                    }
                }
            },
            "hints": {
                "0": [
                    {
                        "accessible_scopes": [
                            "starkware.cairo.common.alloc",
                            "starkware.cairo.common.alloc.alloc"
                        ],
                        "code": "memory[ap] = segments.add()",
                        "flow_tracking_data": {
                            "ap_tracking": {
                                "group": 0,
                                "offset": 0
                            },
                            "reference_ids": {}
                        }
                    }
                ],
                "46": [
                    {
                        "accessible_scopes": [
                            "__main__",
                            "__main__.main"
                        ],
                        "code": "import math",
                        "flow_tracking_data": {
                            "ap_tracking": {
                                "group": 5,
                                "offset": 0
                            },
                            "reference_ids": {}
                        }
                    }
                ]
            },
            "identifiers": {
                "__main__.main": {
                    "decorators": [],
                    "pc": 0,
                    "type": "function"
                },
                "__main__.main.Args": {
                    "full_name": "__main__.main.Args",
                    "members": {},
                    "size": 0,
                    "type": "struct"
                },
                "__main__.main.ImplicitArgs": {
                    "full_name": "__main__.main.ImplicitArgs",
                    "members": {},
                    "size": 0,
                    "type": "struct"
                },
                "__main__.main.Return": {
                    "full_name": "__main__.main.Return",
                    "members": {},
                    "size": 0,
                    "type": "struct"
                },
                "__main__.main.SIZEOF_LOCALS": {
                    "type": "const",
                    "value": 0
                }
            },
            "main_scope": "__main__",
            "prime": "0x800000000000011000000000000000000000000000000000000000000000001",
            "reference_manager": {
                "references": []
            }
        }        
        "#;
        let program: Program = Program::from_string(&json_artifact.to_string(), Some("main"))
            .expect("Failed to deserialize program");

        let builtins: Vec<String> = Vec::new();
        let data: Vec<MaybeRelocatable> = vec![
            MaybeRelocatable::Int(BigInt::from_i64(5189976364521848832).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(1000).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(5189976364521848832).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(2000).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(5201798304953696256).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(2345108766317314046).unwrap()),
        ];

        let mut identifiers: HashMap<String, Identifier> = HashMap::new();

        identifiers.insert(
            String::from("__main__.main"),
            Identifier {
                pc: Some(0),
                type_: Some(String::from("function")),
                value: None,
                full_name: None,
                members: None,
            },
        );
        identifiers.insert(
            String::from("__main__.main.Args"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.Args".to_string()),
                members: Some(HashMap::new()),
            },
        );
        identifiers.insert(
            String::from("__main__.main.ImplicitArgs"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.ImplicitArgs".to_string()),
                members: Some(HashMap::new()),
            },
        );
        identifiers.insert(
            String::from("__main__.main.Return"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.Return".to_string()),
                members: Some(HashMap::new()),
            },
        );
        identifiers.insert(
            String::from("__main__.main.SIZEOF_LOCALS"),
            Identifier {
                pc: None,
                type_: Some(String::from("const")),
                value: Some(bigint!(0)),
                full_name: None,
                members: None,
            },
        );

        assert_eq!(
            program.prime,
            BigInt::parse_bytes(
                b"3618502788666131213697322783095070105623107215331596699973092056135872020481",
                10
            )
            .unwrap()
        );
        assert_eq!(program.builtins, builtins);
        assert_eq!(program.data, data);
        assert_eq!(program.main, Some(0));
        assert_eq!(program.identifiers, identifiers);
    }

    #[test]
    fn deserialize_program_test() {
        let program: Program = Program::from_file(
            Path::new("cairo_programs/manually_compiled/valid_program_a.json"),
            Some("main"),
        )
        .expect("Failed to deserialize program");

        let builtins: Vec<String> = Vec::new();
        let data: Vec<MaybeRelocatable> = vec![
            MaybeRelocatable::Int(BigInt::from_i64(5189976364521848832).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(1000).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(5189976364521848832).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(2000).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(5201798304953696256).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(2345108766317314046).unwrap()),
        ];

        let mut identifiers: HashMap<String, Identifier> = HashMap::new();

        identifiers.insert(
            String::from("__main__.main"),
            Identifier {
                pc: Some(0),
                type_: Some(String::from("function")),
                value: None,
                full_name: None,
                members: None,
            },
        );
        identifiers.insert(
            String::from("__main__.main.Args"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.Args".to_string()),
                members: Some(HashMap::new()),
            },
        );
        identifiers.insert(
            String::from("__main__.main.ImplicitArgs"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.ImplicitArgs".to_string()),
                members: Some(HashMap::new()),
            },
        );
        identifiers.insert(
            String::from("__main__.main.Return"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.Return".to_string()),
                members: Some(HashMap::new()),
            },
        );
        identifiers.insert(
            String::from("__main__.main.SIZEOF_LOCALS"),
            Identifier {
                pc: None,
                type_: Some(String::from("const")),
                value: Some(bigint!(0)),
                full_name: None,
                members: None,
            },
        );

        assert_eq!(
            program.prime,
            BigInt::parse_bytes(
                b"3618502788666131213697322783095070105623107215331596699973092056135872020481",
                10
            )
            .unwrap()
        );
        assert_eq!(program.builtins, builtins);
        assert_eq!(program.data, data);
        assert_eq!(program.main, Some(0));
        assert_eq!(program.identifiers, identifiers);
    }

    /// Deserialize a program without an entrypoint.
    #[test]
    fn deserialize_program_without_entrypoint_test() {
        let program: Program = Program::from_file(
            Path::new("cairo_programs/manually_compiled/valid_program_a.json"),
            None,
        )
        .expect("Failed to deserialize program");

        let builtins: Vec<String> = Vec::new();

        let error_message_attributes: Vec<Attribute> = vec![Attribute {
            name: String::from("error_message"),
            start_pc: 379,
            end_pc: 381,
            value: String::from("SafeUint256: addition overflow"),
        }];

        let data: Vec<MaybeRelocatable> = vec![
            MaybeRelocatable::Int(BigInt::from_i64(5189976364521848832).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(1000).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(5189976364521848832).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(2000).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(5201798304953696256).unwrap()),
            MaybeRelocatable::Int(BigInt::from_i64(2345108766317314046).unwrap()),
        ];

        let mut identifiers: HashMap<String, Identifier> = HashMap::new();

        identifiers.insert(
            String::from("__main__.main"),
            Identifier {
                pc: Some(0),
                type_: Some(String::from("function")),
                value: None,
                full_name: None,
                members: None,
            },
        );
        identifiers.insert(
            String::from("__main__.main.Args"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.Args".to_string()),
                members: Some(HashMap::new()),
            },
        );
        identifiers.insert(
            String::from("__main__.main.ImplicitArgs"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.ImplicitArgs".to_string()),
                members: Some(HashMap::new()),
            },
        );
        identifiers.insert(
            String::from("__main__.main.Return"),
            Identifier {
                pc: None,
                type_: Some(String::from("struct")),
                value: None,
                full_name: Some("__main__.main.Return".to_string()),
                members: Some(HashMap::new()),
            },
        );
        identifiers.insert(
            String::from("__main__.main.SIZEOF_LOCALS"),
            Identifier {
                pc: None,
                type_: Some(String::from("const")),
                value: Some(bigint!(0)),
                full_name: None,
                members: None,
            },
        );

        assert_eq!(
            program.prime,
            BigInt::parse_bytes(
                b"3618502788666131213697322783095070105623107215331596699973092056135872020481",
                10
            )
            .unwrap()
        );
        assert_eq!(program.builtins, builtins);
        assert_eq!(program.data, data);
        assert_eq!(program.main, None);
        assert_eq!(program.identifiers, identifiers);
        assert_eq!(program.error_message_attributes, error_message_attributes)
    }

    #[test]
    fn deserialize_program_constants_test() {
        let program = Program::from_file(
            Path::new("cairo_programs/manually_compiled/deserialize_constant_test.json"),
            Some("main"),
        )
        .expect("Failed to deserialize program");

        let constants = [
            (
                "__main__.compare_abs_arrays.SIZEOF_LOCALS",
                bigint_str!(
                    b"-3618502788666131213697322783095070105623107215331596699973092056135872020481"
                ),
            ),
            (
                "starkware.cairo.common.cairo_keccak.packed_keccak.ALL_ONES",
                bigint_str!(b"-106710729501573572985208420194530329073740042555888586719234"),
            ),
            (
                "starkware.cairo.common.cairo_keccak.packed_keccak.BLOCK_SIZE",
                bigint!(3),
            ),
            (
                "starkware.cairo.common.alloc.alloc.SIZEOF_LOCALS",
                bigint!(0),
            ),
            (
                "starkware.cairo.common.uint256.SHIFT",
                bigint_str!(b"340282366920938463463374607431768211456"),
            ),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect::<HashMap<_, _>>();

        assert_eq!(program.constants, constants);
    }

    #[test]
    fn default_program() {
        let program = Program {
            builtins: Vec::new(),
            prime: BigInt::new(Sign::Plus, vec![1, 0, 0, 0, 0, 0, 17, 134217728]),
            data: Vec::new(),
            constants: HashMap::new(),
            main: None,
            start: None,
            end: None,
            hints: HashMap::new(),
            reference_manager: ReferenceManager {
                references: Vec::new(),
            },
            identifiers: HashMap::new(),
            error_message_attributes: Vec::new(),
        };

        assert_eq!(program, Program::default())
    }
}
