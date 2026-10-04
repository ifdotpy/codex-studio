// Generated from the Python OpenAPI contract. Do not edit.
/* oxlint-disable no-unused-vars, no-unreachable -- unmodified Ajv compiler output */
var __getOwnPropNames = Object.getOwnPropertyNames;
var __commonJS = (cb, mod) =>
  function __require() {
    try {
      return (
        mod ||
          (0, cb[__getOwnPropNames(cb)[0]])(
            (mod = { exports: {} }).exports,
            mod,
          ),
        mod.exports
      );
    } catch (e) {
      throw ((mod = 0), e);
    }
  };

// web/node_modules/ajv/dist/runtime/ucs2length.js
var require_ucs2length = __commonJS({
  "web/node_modules/ajv/dist/runtime/ucs2length.js"(exports) {
    "use strict";
    Object.defineProperty(exports, "__esModule", { value: true });
    function ucs2length(str) {
      const len = str.length;
      let length = 0;
      let pos = 0;
      let value;
      while (pos < len) {
        length++;
        value = str.charCodeAt(pos++);
        if (value >= 55296 && value <= 56319 && pos < len) {
          value = str.charCodeAt(pos);
          if ((value & 64512) === 56320) pos++;
        }
      }
      return length;
    }
    exports.default = ucs2length;
    ucs2length.code = 'require("ajv/dist/runtime/ucs2length").default';
  },
});

// web/stream-validators.js
var isResourceRef = validate21;
var func1 = require_ucs2length().default;
function validate21(
  data,
  {
    instancePath = "",
    parentData,
    parentDataProperty,
    rootData = data,
    dynamicAnchors = {},
  } = {},
) {
  let vErrors = null;
  let errors = 0;
  const evaluated0 = validate21.evaluated;
  if (evaluated0.dynamicProps) {
    evaluated0.props = void 0;
  }
  if (evaluated0.dynamicItems) {
    evaluated0.items = void 0;
  }
  const _errs0 = errors;
  let valid0 = false;
  let passing0 = null;
  const _errs1 = errors;
  const _errs2 = errors;
  if (errors === _errs2) {
    if (data && typeof data == "object" && !Array.isArray(data)) {
      let missing0;
      if (
        (data.kind === void 0 && (missing0 = "kind")) ||
        (data.agentId === void 0 && (missing0 = "agentId"))
      ) {
        const err0 = {
          instancePath,
          schemaPath: "#/$defs/PanelResource/required",
          keyword: "required",
          params: { missingProperty: missing0 },
          message: "must have required property '" + missing0 + "'",
        };
        if (vErrors === null) {
          vErrors = [err0];
        } else {
          vErrors.push(err0);
        }
        errors++;
      } else {
        const _errs4 = errors;
        for (const key0 in data) {
          if (!(key0 === "kind" || key0 === "agentId")) {
            const err1 = {
              instancePath,
              schemaPath: "#/$defs/PanelResource/additionalProperties",
              keyword: "additionalProperties",
              params: { additionalProperty: key0 },
              message: "must NOT have additional properties",
            };
            if (vErrors === null) {
              vErrors = [err1];
            } else {
              vErrors.push(err1);
            }
            errors++;
            break;
          }
        }
        if (_errs4 === errors) {
          if (data.kind !== void 0) {
            let data0 = data.kind;
            const _errs5 = errors;
            if (typeof data0 !== "string") {
              const err2 = {
                instancePath: instancePath + "/kind",
                schemaPath: "#/$defs/PanelResource/properties/kind/type",
                keyword: "type",
                params: { type: "string" },
                message: "must be string",
              };
              if (vErrors === null) {
                vErrors = [err2];
              } else {
                vErrors.push(err2);
              }
              errors++;
            }
            if ("panel" !== data0) {
              const err3 = {
                instancePath: instancePath + "/kind",
                schemaPath: "#/$defs/PanelResource/properties/kind/const",
                keyword: "const",
                params: { allowedValue: "panel" },
                message: "must be equal to constant",
              };
              if (vErrors === null) {
                vErrors = [err3];
              } else {
                vErrors.push(err3);
              }
              errors++;
            }
            var valid2 = _errs5 === errors;
          } else {
            var valid2 = true;
          }
          if (valid2) {
            if (data.agentId !== void 0) {
              let data1 = data.agentId;
              const _errs7 = errors;
              if (errors === _errs7) {
                if (typeof data1 === "string") {
                  if (func1(data1) < 1) {
                    const err4 = {
                      instancePath: instancePath + "/agentId",
                      schemaPath:
                        "#/$defs/PanelResource/properties/agentId/minLength",
                      keyword: "minLength",
                      params: { limit: 1 },
                      message: "must NOT have fewer than 1 characters",
                    };
                    if (vErrors === null) {
                      vErrors = [err4];
                    } else {
                      vErrors.push(err4);
                    }
                    errors++;
                  }
                } else {
                  const err5 = {
                    instancePath: instancePath + "/agentId",
                    schemaPath: "#/$defs/PanelResource/properties/agentId/type",
                    keyword: "type",
                    params: { type: "string" },
                    message: "must be string",
                  };
                  if (vErrors === null) {
                    vErrors = [err5];
                  } else {
                    vErrors.push(err5);
                  }
                  errors++;
                }
              }
              var valid2 = _errs7 === errors;
            } else {
              var valid2 = true;
            }
          }
        }
      }
    } else {
      const err6 = {
        instancePath,
        schemaPath: "#/$defs/PanelResource/type",
        keyword: "type",
        params: { type: "object" },
        message: "must be object",
      };
      if (vErrors === null) {
        vErrors = [err6];
      } else {
        vErrors.push(err6);
      }
      errors++;
    }
  }
  var _valid0 = _errs1 === errors;
  if (_valid0) {
    valid0 = true;
    passing0 = 0;
    var props0 = true;
  }
  const _errs9 = errors;
  const _errs10 = errors;
  if (errors === _errs10) {
    if (data && typeof data == "object" && !Array.isArray(data)) {
      let missing1;
      if (
        (data.kind === void 0 && (missing1 = "kind")) ||
        (data.agentId === void 0 && (missing1 = "agentId"))
      ) {
        const err7 = {
          instancePath,
          schemaPath: "#/$defs/QueueResource/required",
          keyword: "required",
          params: { missingProperty: missing1 },
          message: "must have required property '" + missing1 + "'",
        };
        if (vErrors === null) {
          vErrors = [err7];
        } else {
          vErrors.push(err7);
        }
        errors++;
      } else {
        const _errs12 = errors;
        for (const key1 in data) {
          if (!(key1 === "kind" || key1 === "agentId")) {
            const err8 = {
              instancePath,
              schemaPath: "#/$defs/QueueResource/additionalProperties",
              keyword: "additionalProperties",
              params: { additionalProperty: key1 },
              message: "must NOT have additional properties",
            };
            if (vErrors === null) {
              vErrors = [err8];
            } else {
              vErrors.push(err8);
            }
            errors++;
            break;
          }
        }
        if (_errs12 === errors) {
          if (data.kind !== void 0) {
            let data2 = data.kind;
            const _errs13 = errors;
            if (typeof data2 !== "string") {
              const err9 = {
                instancePath: instancePath + "/kind",
                schemaPath: "#/$defs/QueueResource/properties/kind/type",
                keyword: "type",
                params: { type: "string" },
                message: "must be string",
              };
              if (vErrors === null) {
                vErrors = [err9];
              } else {
                vErrors.push(err9);
              }
              errors++;
            }
            if ("queue" !== data2) {
              const err10 = {
                instancePath: instancePath + "/kind",
                schemaPath: "#/$defs/QueueResource/properties/kind/const",
                keyword: "const",
                params: { allowedValue: "queue" },
                message: "must be equal to constant",
              };
              if (vErrors === null) {
                vErrors = [err10];
              } else {
                vErrors.push(err10);
              }
              errors++;
            }
            var valid4 = _errs13 === errors;
          } else {
            var valid4 = true;
          }
          if (valid4) {
            if (data.agentId !== void 0) {
              let data3 = data.agentId;
              const _errs15 = errors;
              if (errors === _errs15) {
                if (typeof data3 === "string") {
                  if (func1(data3) < 1) {
                    const err11 = {
                      instancePath: instancePath + "/agentId",
                      schemaPath:
                        "#/$defs/QueueResource/properties/agentId/minLength",
                      keyword: "minLength",
                      params: { limit: 1 },
                      message: "must NOT have fewer than 1 characters",
                    };
                    if (vErrors === null) {
                      vErrors = [err11];
                    } else {
                      vErrors.push(err11);
                    }
                    errors++;
                  }
                } else {
                  const err12 = {
                    instancePath: instancePath + "/agentId",
                    schemaPath: "#/$defs/QueueResource/properties/agentId/type",
                    keyword: "type",
                    params: { type: "string" },
                    message: "must be string",
                  };
                  if (vErrors === null) {
                    vErrors = [err12];
                  } else {
                    vErrors.push(err12);
                  }
                  errors++;
                }
              }
              var valid4 = _errs15 === errors;
            } else {
              var valid4 = true;
            }
          }
        }
      }
    } else {
      const err13 = {
        instancePath,
        schemaPath: "#/$defs/QueueResource/type",
        keyword: "type",
        params: { type: "object" },
        message: "must be object",
      };
      if (vErrors === null) {
        vErrors = [err13];
      } else {
        vErrors.push(err13);
      }
      errors++;
    }
  }
  var _valid0 = _errs9 === errors;
  if (_valid0 && valid0) {
    valid0 = false;
    passing0 = [passing0, 1];
  } else {
    if (_valid0) {
      valid0 = true;
      passing0 = 1;
      if (props0 !== true) {
        props0 = true;
      }
    }
    const _errs17 = errors;
    const _errs18 = errors;
    if (errors === _errs18) {
      if (data && typeof data == "object" && !Array.isArray(data)) {
        let missing2;
        if (
          (data.kind === void 0 && (missing2 = "kind")) ||
          (data.agentId === void 0 && (missing2 = "agentId"))
        ) {
          const err14 = {
            instancePath,
            schemaPath: "#/$defs/ReceiptsResource/required",
            keyword: "required",
            params: { missingProperty: missing2 },
            message: "must have required property '" + missing2 + "'",
          };
          if (vErrors === null) {
            vErrors = [err14];
          } else {
            vErrors.push(err14);
          }
          errors++;
        } else {
          const _errs20 = errors;
          for (const key2 in data) {
            if (!(key2 === "kind" || key2 === "agentId")) {
              const err15 = {
                instancePath,
                schemaPath: "#/$defs/ReceiptsResource/additionalProperties",
                keyword: "additionalProperties",
                params: { additionalProperty: key2 },
                message: "must NOT have additional properties",
              };
              if (vErrors === null) {
                vErrors = [err15];
              } else {
                vErrors.push(err15);
              }
              errors++;
              break;
            }
          }
          if (_errs20 === errors) {
            if (data.kind !== void 0) {
              let data4 = data.kind;
              const _errs21 = errors;
              if (typeof data4 !== "string") {
                const err16 = {
                  instancePath: instancePath + "/kind",
                  schemaPath: "#/$defs/ReceiptsResource/properties/kind/type",
                  keyword: "type",
                  params: { type: "string" },
                  message: "must be string",
                };
                if (vErrors === null) {
                  vErrors = [err16];
                } else {
                  vErrors.push(err16);
                }
                errors++;
              }
              if ("receipts" !== data4) {
                const err17 = {
                  instancePath: instancePath + "/kind",
                  schemaPath: "#/$defs/ReceiptsResource/properties/kind/const",
                  keyword: "const",
                  params: { allowedValue: "receipts" },
                  message: "must be equal to constant",
                };
                if (vErrors === null) {
                  vErrors = [err17];
                } else {
                  vErrors.push(err17);
                }
                errors++;
              }
              var valid6 = _errs21 === errors;
            } else {
              var valid6 = true;
            }
            if (valid6) {
              if (data.agentId !== void 0) {
                let data5 = data.agentId;
                const _errs23 = errors;
                if (errors === _errs23) {
                  if (typeof data5 === "string") {
                    if (func1(data5) < 1) {
                      const err18 = {
                        instancePath: instancePath + "/agentId",
                        schemaPath:
                          "#/$defs/ReceiptsResource/properties/agentId/minLength",
                        keyword: "minLength",
                        params: { limit: 1 },
                        message: "must NOT have fewer than 1 characters",
                      };
                      if (vErrors === null) {
                        vErrors = [err18];
                      } else {
                        vErrors.push(err18);
                      }
                      errors++;
                    }
                  } else {
                    const err19 = {
                      instancePath: instancePath + "/agentId",
                      schemaPath:
                        "#/$defs/ReceiptsResource/properties/agentId/type",
                      keyword: "type",
                      params: { type: "string" },
                      message: "must be string",
                    };
                    if (vErrors === null) {
                      vErrors = [err19];
                    } else {
                      vErrors.push(err19);
                    }
                    errors++;
                  }
                }
                var valid6 = _errs23 === errors;
              } else {
                var valid6 = true;
              }
            }
          }
        }
      } else {
        const err20 = {
          instancePath,
          schemaPath: "#/$defs/ReceiptsResource/type",
          keyword: "type",
          params: { type: "object" },
          message: "must be object",
        };
        if (vErrors === null) {
          vErrors = [err20];
        } else {
          vErrors.push(err20);
        }
        errors++;
      }
    }
    var _valid0 = _errs17 === errors;
    if (_valid0 && valid0) {
      valid0 = false;
      passing0 = [passing0, 2];
    } else {
      if (_valid0) {
        valid0 = true;
        passing0 = 2;
        if (props0 !== true) {
          props0 = true;
        }
      }
      const _errs25 = errors;
      const _errs26 = errors;
      if (errors === _errs26) {
        if (data && typeof data == "object" && !Array.isArray(data)) {
          let missing3;
          if (
            (data.kind === void 0 && (missing3 = "kind")) ||
            (data.terminalId === void 0 && (missing3 = "terminalId"))
          ) {
            const err21 = {
              instancePath,
              schemaPath: "#/$defs/TerminalResource/required",
              keyword: "required",
              params: { missingProperty: missing3 },
              message: "must have required property '" + missing3 + "'",
            };
            if (vErrors === null) {
              vErrors = [err21];
            } else {
              vErrors.push(err21);
            }
            errors++;
          } else {
            const _errs28 = errors;
            for (const key3 in data) {
              if (!(key3 === "kind" || key3 === "terminalId")) {
                const err22 = {
                  instancePath,
                  schemaPath: "#/$defs/TerminalResource/additionalProperties",
                  keyword: "additionalProperties",
                  params: { additionalProperty: key3 },
                  message: "must NOT have additional properties",
                };
                if (vErrors === null) {
                  vErrors = [err22];
                } else {
                  vErrors.push(err22);
                }
                errors++;
                break;
              }
            }
            if (_errs28 === errors) {
              if (data.kind !== void 0) {
                let data6 = data.kind;
                const _errs29 = errors;
                if (typeof data6 !== "string") {
                  const err23 = {
                    instancePath: instancePath + "/kind",
                    schemaPath: "#/$defs/TerminalResource/properties/kind/type",
                    keyword: "type",
                    params: { type: "string" },
                    message: "must be string",
                  };
                  if (vErrors === null) {
                    vErrors = [err23];
                  } else {
                    vErrors.push(err23);
                  }
                  errors++;
                }
                if ("terminal" !== data6) {
                  const err24 = {
                    instancePath: instancePath + "/kind",
                    schemaPath:
                      "#/$defs/TerminalResource/properties/kind/const",
                    keyword: "const",
                    params: { allowedValue: "terminal" },
                    message: "must be equal to constant",
                  };
                  if (vErrors === null) {
                    vErrors = [err24];
                  } else {
                    vErrors.push(err24);
                  }
                  errors++;
                }
                var valid8 = _errs29 === errors;
              } else {
                var valid8 = true;
              }
              if (valid8) {
                if (data.terminalId !== void 0) {
                  let data7 = data.terminalId;
                  const _errs31 = errors;
                  if (errors === _errs31) {
                    if (typeof data7 === "string") {
                      if (func1(data7) < 1) {
                        const err25 = {
                          instancePath: instancePath + "/terminalId",
                          schemaPath:
                            "#/$defs/TerminalResource/properties/terminalId/minLength",
                          keyword: "minLength",
                          params: { limit: 1 },
                          message: "must NOT have fewer than 1 characters",
                        };
                        if (vErrors === null) {
                          vErrors = [err25];
                        } else {
                          vErrors.push(err25);
                        }
                        errors++;
                      }
                    } else {
                      const err26 = {
                        instancePath: instancePath + "/terminalId",
                        schemaPath:
                          "#/$defs/TerminalResource/properties/terminalId/type",
                        keyword: "type",
                        params: { type: "string" },
                        message: "must be string",
                      };
                      if (vErrors === null) {
                        vErrors = [err26];
                      } else {
                        vErrors.push(err26);
                      }
                      errors++;
                    }
                  }
                  var valid8 = _errs31 === errors;
                } else {
                  var valid8 = true;
                }
              }
            }
          }
        } else {
          const err27 = {
            instancePath,
            schemaPath: "#/$defs/TerminalResource/type",
            keyword: "type",
            params: { type: "object" },
            message: "must be object",
          };
          if (vErrors === null) {
            vErrors = [err27];
          } else {
            vErrors.push(err27);
          }
          errors++;
        }
      }
      var _valid0 = _errs25 === errors;
      if (_valid0 && valid0) {
        valid0 = false;
        passing0 = [passing0, 3];
      } else {
        if (_valid0) {
          valid0 = true;
          passing0 = 3;
          if (props0 !== true) {
            props0 = true;
          }
        }
        const _errs33 = errors;
        const _errs34 = errors;
        if (errors === _errs34) {
          if (data && typeof data == "object" && !Array.isArray(data)) {
            let missing4;
            if (data.kind === void 0 && (missing4 = "kind")) {
              const err28 = {
                instancePath,
                schemaPath: "#/$defs/TerminalsResource/required",
                keyword: "required",
                params: { missingProperty: missing4 },
                message: "must have required property '" + missing4 + "'",
              };
              if (vErrors === null) {
                vErrors = [err28];
              } else {
                vErrors.push(err28);
              }
              errors++;
            } else {
              const _errs36 = errors;
              for (const key4 in data) {
                if (!(key4 === "kind")) {
                  const err29 = {
                    instancePath,
                    schemaPath:
                      "#/$defs/TerminalsResource/additionalProperties",
                    keyword: "additionalProperties",
                    params: { additionalProperty: key4 },
                    message: "must NOT have additional properties",
                  };
                  if (vErrors === null) {
                    vErrors = [err29];
                  } else {
                    vErrors.push(err29);
                  }
                  errors++;
                  break;
                }
              }
              if (_errs36 === errors) {
                if (data.kind !== void 0) {
                  let data8 = data.kind;
                  if (typeof data8 !== "string") {
                    const err30 = {
                      instancePath: instancePath + "/kind",
                      schemaPath:
                        "#/$defs/TerminalsResource/properties/kind/type",
                      keyword: "type",
                      params: { type: "string" },
                      message: "must be string",
                    };
                    if (vErrors === null) {
                      vErrors = [err30];
                    } else {
                      vErrors.push(err30);
                    }
                    errors++;
                  }
                  if ("terminals" !== data8) {
                    const err31 = {
                      instancePath: instancePath + "/kind",
                      schemaPath:
                        "#/$defs/TerminalsResource/properties/kind/const",
                      keyword: "const",
                      params: { allowedValue: "terminals" },
                      message: "must be equal to constant",
                    };
                    if (vErrors === null) {
                      vErrors = [err31];
                    } else {
                      vErrors.push(err31);
                    }
                    errors++;
                  }
                }
              }
            }
          } else {
            const err32 = {
              instancePath,
              schemaPath: "#/$defs/TerminalsResource/type",
              keyword: "type",
              params: { type: "object" },
              message: "must be object",
            };
            if (vErrors === null) {
              vErrors = [err32];
            } else {
              vErrors.push(err32);
            }
            errors++;
          }
        }
        var _valid0 = _errs33 === errors;
        if (_valid0 && valid0) {
          valid0 = false;
          passing0 = [passing0, 4];
        } else {
          if (_valid0) {
            valid0 = true;
            passing0 = 4;
            if (props0 !== true) {
              props0 = true;
            }
          }
          const _errs39 = errors;
          const _errs40 = errors;
          if (errors === _errs40) {
            if (data && typeof data == "object" && !Array.isArray(data)) {
              let missing5;
              if (data.kind === void 0 && (missing5 = "kind")) {
                const err33 = {
                  instancePath,
                  schemaPath: "#/$defs/AccountsResource/required",
                  keyword: "required",
                  params: { missingProperty: missing5 },
                  message: "must have required property '" + missing5 + "'",
                };
                if (vErrors === null) {
                  vErrors = [err33];
                } else {
                  vErrors.push(err33);
                }
                errors++;
              } else {
                const _errs42 = errors;
                for (const key5 in data) {
                  if (!(key5 === "kind")) {
                    const err34 = {
                      instancePath,
                      schemaPath:
                        "#/$defs/AccountsResource/additionalProperties",
                      keyword: "additionalProperties",
                      params: { additionalProperty: key5 },
                      message: "must NOT have additional properties",
                    };
                    if (vErrors === null) {
                      vErrors = [err34];
                    } else {
                      vErrors.push(err34);
                    }
                    errors++;
                    break;
                  }
                }
                if (_errs42 === errors) {
                  if (data.kind !== void 0) {
                    let data9 = data.kind;
                    if (typeof data9 !== "string") {
                      const err35 = {
                        instancePath: instancePath + "/kind",
                        schemaPath:
                          "#/$defs/AccountsResource/properties/kind/type",
                        keyword: "type",
                        params: { type: "string" },
                        message: "must be string",
                      };
                      if (vErrors === null) {
                        vErrors = [err35];
                      } else {
                        vErrors.push(err35);
                      }
                      errors++;
                    }
                    if ("accounts" !== data9) {
                      const err36 = {
                        instancePath: instancePath + "/kind",
                        schemaPath:
                          "#/$defs/AccountsResource/properties/kind/const",
                        keyword: "const",
                        params: { allowedValue: "accounts" },
                        message: "must be equal to constant",
                      };
                      if (vErrors === null) {
                        vErrors = [err36];
                      } else {
                        vErrors.push(err36);
                      }
                      errors++;
                    }
                  }
                }
              }
            } else {
              const err37 = {
                instancePath,
                schemaPath: "#/$defs/AccountsResource/type",
                keyword: "type",
                params: { type: "object" },
                message: "must be object",
              };
              if (vErrors === null) {
                vErrors = [err37];
              } else {
                vErrors.push(err37);
              }
              errors++;
            }
          }
          var _valid0 = _errs39 === errors;
          if (_valid0 && valid0) {
            valid0 = false;
            passing0 = [passing0, 5];
          } else {
            if (_valid0) {
              valid0 = true;
              passing0 = 5;
              if (props0 !== true) {
                props0 = true;
              }
            }
            const _errs45 = errors;
            const _errs46 = errors;
            if (errors === _errs46) {
              if (data && typeof data == "object" && !Array.isArray(data)) {
                let missing6;
                if (
                  (data.kind === void 0 && (missing6 = "kind")) ||
                  (data.accountKey === void 0 && (missing6 = "accountKey"))
                ) {
                  const err38 = {
                    instancePath,
                    schemaPath: "#/$defs/LimitsResource/required",
                    keyword: "required",
                    params: { missingProperty: missing6 },
                    message: "must have required property '" + missing6 + "'",
                  };
                  if (vErrors === null) {
                    vErrors = [err38];
                  } else {
                    vErrors.push(err38);
                  }
                  errors++;
                } else {
                  const _errs48 = errors;
                  for (const key6 in data) {
                    if (!(key6 === "kind" || key6 === "accountKey")) {
                      const err39 = {
                        instancePath,
                        schemaPath:
                          "#/$defs/LimitsResource/additionalProperties",
                        keyword: "additionalProperties",
                        params: { additionalProperty: key6 },
                        message: "must NOT have additional properties",
                      };
                      if (vErrors === null) {
                        vErrors = [err39];
                      } else {
                        vErrors.push(err39);
                      }
                      errors++;
                      break;
                    }
                  }
                  if (_errs48 === errors) {
                    if (data.kind !== void 0) {
                      let data10 = data.kind;
                      const _errs49 = errors;
                      if (typeof data10 !== "string") {
                        const err40 = {
                          instancePath: instancePath + "/kind",
                          schemaPath:
                            "#/$defs/LimitsResource/properties/kind/type",
                          keyword: "type",
                          params: { type: "string" },
                          message: "must be string",
                        };
                        if (vErrors === null) {
                          vErrors = [err40];
                        } else {
                          vErrors.push(err40);
                        }
                        errors++;
                      }
                      if ("limits" !== data10) {
                        const err41 = {
                          instancePath: instancePath + "/kind",
                          schemaPath:
                            "#/$defs/LimitsResource/properties/kind/const",
                          keyword: "const",
                          params: { allowedValue: "limits" },
                          message: "must be equal to constant",
                        };
                        if (vErrors === null) {
                          vErrors = [err41];
                        } else {
                          vErrors.push(err41);
                        }
                        errors++;
                      }
                      var valid14 = _errs49 === errors;
                    } else {
                      var valid14 = true;
                    }
                    if (valid14) {
                      if (data.accountKey !== void 0) {
                        let data11 = data.accountKey;
                        const _errs51 = errors;
                        if (errors === _errs51) {
                          if (typeof data11 === "string") {
                            if (func1(data11) < 1) {
                              const err42 = {
                                instancePath: instancePath + "/accountKey",
                                schemaPath:
                                  "#/$defs/LimitsResource/properties/accountKey/minLength",
                                keyword: "minLength",
                                params: { limit: 1 },
                                message:
                                  "must NOT have fewer than 1 characters",
                              };
                              if (vErrors === null) {
                                vErrors = [err42];
                              } else {
                                vErrors.push(err42);
                              }
                              errors++;
                            }
                          } else {
                            const err43 = {
                              instancePath: instancePath + "/accountKey",
                              schemaPath:
                                "#/$defs/LimitsResource/properties/accountKey/type",
                              keyword: "type",
                              params: { type: "string" },
                              message: "must be string",
                            };
                            if (vErrors === null) {
                              vErrors = [err43];
                            } else {
                              vErrors.push(err43);
                            }
                            errors++;
                          }
                        }
                        var valid14 = _errs51 === errors;
                      } else {
                        var valid14 = true;
                      }
                    }
                  }
                }
              } else {
                const err44 = {
                  instancePath,
                  schemaPath: "#/$defs/LimitsResource/type",
                  keyword: "type",
                  params: { type: "object" },
                  message: "must be object",
                };
                if (vErrors === null) {
                  vErrors = [err44];
                } else {
                  vErrors.push(err44);
                }
                errors++;
              }
            }
            var _valid0 = _errs45 === errors;
            if (_valid0 && valid0) {
              valid0 = false;
              passing0 = [passing0, 6];
            } else {
              if (_valid0) {
                valid0 = true;
                passing0 = 6;
                if (props0 !== true) {
                  props0 = true;
                }
              }
              const _errs53 = errors;
              const _errs54 = errors;
              if (errors === _errs54) {
                if (data && typeof data == "object" && !Array.isArray(data)) {
                  let missing7;
                  if (data.kind === void 0 && (missing7 = "kind")) {
                    const err45 = {
                      instancePath,
                      schemaPath: "#/$defs/ModelsResource/required",
                      keyword: "required",
                      params: { missingProperty: missing7 },
                      message: "must have required property '" + missing7 + "'",
                    };
                    if (vErrors === null) {
                      vErrors = [err45];
                    } else {
                      vErrors.push(err45);
                    }
                    errors++;
                  } else {
                    const _errs56 = errors;
                    for (const key7 in data) {
                      if (!(key7 === "kind")) {
                        const err46 = {
                          instancePath,
                          schemaPath:
                            "#/$defs/ModelsResource/additionalProperties",
                          keyword: "additionalProperties",
                          params: { additionalProperty: key7 },
                          message: "must NOT have additional properties",
                        };
                        if (vErrors === null) {
                          vErrors = [err46];
                        } else {
                          vErrors.push(err46);
                        }
                        errors++;
                        break;
                      }
                    }
                    if (_errs56 === errors) {
                      if (data.kind !== void 0) {
                        let data12 = data.kind;
                        if (typeof data12 !== "string") {
                          const err47 = {
                            instancePath: instancePath + "/kind",
                            schemaPath:
                              "#/$defs/ModelsResource/properties/kind/type",
                            keyword: "type",
                            params: { type: "string" },
                            message: "must be string",
                          };
                          if (vErrors === null) {
                            vErrors = [err47];
                          } else {
                            vErrors.push(err47);
                          }
                          errors++;
                        }
                        if ("models" !== data12) {
                          const err48 = {
                            instancePath: instancePath + "/kind",
                            schemaPath:
                              "#/$defs/ModelsResource/properties/kind/const",
                            keyword: "const",
                            params: { allowedValue: "models" },
                            message: "must be equal to constant",
                          };
                          if (vErrors === null) {
                            vErrors = [err48];
                          } else {
                            vErrors.push(err48);
                          }
                          errors++;
                        }
                      }
                    }
                  }
                } else {
                  const err49 = {
                    instancePath,
                    schemaPath: "#/$defs/ModelsResource/type",
                    keyword: "type",
                    params: { type: "object" },
                    message: "must be object",
                  };
                  if (vErrors === null) {
                    vErrors = [err49];
                  } else {
                    vErrors.push(err49);
                  }
                  errors++;
                }
              }
              var _valid0 = _errs53 === errors;
              if (_valid0 && valid0) {
                valid0 = false;
                passing0 = [passing0, 7];
              } else {
                if (_valid0) {
                  valid0 = true;
                  passing0 = 7;
                  if (props0 !== true) {
                    props0 = true;
                  }
                }
                const _errs59 = errors;
                const _errs60 = errors;
                if (errors === _errs60) {
                  if (data && typeof data == "object" && !Array.isArray(data)) {
                    let missing8;
                    if (
                      (data.kind === void 0 && (missing8 = "kind")) ||
                      (data.agentId === void 0 && (missing8 = "agentId"))
                    ) {
                      const err50 = {
                        instancePath,
                        schemaPath: "#/$defs/TasksResource/required",
                        keyword: "required",
                        params: { missingProperty: missing8 },
                        message:
                          "must have required property '" + missing8 + "'",
                      };
                      if (vErrors === null) {
                        vErrors = [err50];
                      } else {
                        vErrors.push(err50);
                      }
                      errors++;
                    } else {
                      const _errs62 = errors;
                      for (const key8 in data) {
                        if (!(key8 === "kind" || key8 === "agentId")) {
                          const err51 = {
                            instancePath,
                            schemaPath:
                              "#/$defs/TasksResource/additionalProperties",
                            keyword: "additionalProperties",
                            params: { additionalProperty: key8 },
                            message: "must NOT have additional properties",
                          };
                          if (vErrors === null) {
                            vErrors = [err51];
                          } else {
                            vErrors.push(err51);
                          }
                          errors++;
                          break;
                        }
                      }
                      if (_errs62 === errors) {
                        if (data.kind !== void 0) {
                          let data13 = data.kind;
                          const _errs63 = errors;
                          if (typeof data13 !== "string") {
                            const err52 = {
                              instancePath: instancePath + "/kind",
                              schemaPath:
                                "#/$defs/TasksResource/properties/kind/type",
                              keyword: "type",
                              params: { type: "string" },
                              message: "must be string",
                            };
                            if (vErrors === null) {
                              vErrors = [err52];
                            } else {
                              vErrors.push(err52);
                            }
                            errors++;
                          }
                          if ("tasks" !== data13) {
                            const err53 = {
                              instancePath: instancePath + "/kind",
                              schemaPath:
                                "#/$defs/TasksResource/properties/kind/const",
                              keyword: "const",
                              params: { allowedValue: "tasks" },
                              message: "must be equal to constant",
                            };
                            if (vErrors === null) {
                              vErrors = [err53];
                            } else {
                              vErrors.push(err53);
                            }
                            errors++;
                          }
                          var valid18 = _errs63 === errors;
                        } else {
                          var valid18 = true;
                        }
                        if (valid18) {
                          if (data.agentId !== void 0) {
                            let data14 = data.agentId;
                            const _errs65 = errors;
                            if (errors === _errs65) {
                              if (typeof data14 === "string") {
                                if (func1(data14) < 1) {
                                  const err54 = {
                                    instancePath: instancePath + "/agentId",
                                    schemaPath:
                                      "#/$defs/TasksResource/properties/agentId/minLength",
                                    keyword: "minLength",
                                    params: { limit: 1 },
                                    message:
                                      "must NOT have fewer than 1 characters",
                                  };
                                  if (vErrors === null) {
                                    vErrors = [err54];
                                  } else {
                                    vErrors.push(err54);
                                  }
                                  errors++;
                                }
                              } else {
                                const err55 = {
                                  instancePath: instancePath + "/agentId",
                                  schemaPath:
                                    "#/$defs/TasksResource/properties/agentId/type",
                                  keyword: "type",
                                  params: { type: "string" },
                                  message: "must be string",
                                };
                                if (vErrors === null) {
                                  vErrors = [err55];
                                } else {
                                  vErrors.push(err55);
                                }
                                errors++;
                              }
                            }
                            var valid18 = _errs65 === errors;
                          } else {
                            var valid18 = true;
                          }
                        }
                      }
                    }
                  } else {
                    const err56 = {
                      instancePath,
                      schemaPath: "#/$defs/TasksResource/type",
                      keyword: "type",
                      params: { type: "object" },
                      message: "must be object",
                    };
                    if (vErrors === null) {
                      vErrors = [err56];
                    } else {
                      vErrors.push(err56);
                    }
                    errors++;
                  }
                }
                var _valid0 = _errs59 === errors;
                if (_valid0 && valid0) {
                  valid0 = false;
                  passing0 = [passing0, 8];
                } else {
                  if (_valid0) {
                    valid0 = true;
                    passing0 = 8;
                    if (props0 !== true) {
                      props0 = true;
                    }
                  }
                  const _errs67 = errors;
                  const _errs68 = errors;
                  if (errors === _errs68) {
                    if (
                      data &&
                      typeof data == "object" &&
                      !Array.isArray(data)
                    ) {
                      let missing9;
                      if (
                        (data.kind === void 0 && (missing9 = "kind")) ||
                        (data.taskId === void 0 && (missing9 = "taskId"))
                      ) {
                        const err57 = {
                          instancePath,
                          schemaPath: "#/$defs/TaskResource/required",
                          keyword: "required",
                          params: { missingProperty: missing9 },
                          message:
                            "must have required property '" + missing9 + "'",
                        };
                        if (vErrors === null) {
                          vErrors = [err57];
                        } else {
                          vErrors.push(err57);
                        }
                        errors++;
                      } else {
                        const _errs70 = errors;
                        for (const key9 in data) {
                          if (!(key9 === "kind" || key9 === "taskId")) {
                            const err58 = {
                              instancePath,
                              schemaPath:
                                "#/$defs/TaskResource/additionalProperties",
                              keyword: "additionalProperties",
                              params: { additionalProperty: key9 },
                              message: "must NOT have additional properties",
                            };
                            if (vErrors === null) {
                              vErrors = [err58];
                            } else {
                              vErrors.push(err58);
                            }
                            errors++;
                            break;
                          }
                        }
                        if (_errs70 === errors) {
                          if (data.kind !== void 0) {
                            let data15 = data.kind;
                            const _errs71 = errors;
                            if (typeof data15 !== "string") {
                              const err59 = {
                                instancePath: instancePath + "/kind",
                                schemaPath:
                                  "#/$defs/TaskResource/properties/kind/type",
                                keyword: "type",
                                params: { type: "string" },
                                message: "must be string",
                              };
                              if (vErrors === null) {
                                vErrors = [err59];
                              } else {
                                vErrors.push(err59);
                              }
                              errors++;
                            }
                            if ("task" !== data15) {
                              const err60 = {
                                instancePath: instancePath + "/kind",
                                schemaPath:
                                  "#/$defs/TaskResource/properties/kind/const",
                                keyword: "const",
                                params: { allowedValue: "task" },
                                message: "must be equal to constant",
                              };
                              if (vErrors === null) {
                                vErrors = [err60];
                              } else {
                                vErrors.push(err60);
                              }
                              errors++;
                            }
                            var valid20 = _errs71 === errors;
                          } else {
                            var valid20 = true;
                          }
                          if (valid20) {
                            if (data.taskId !== void 0) {
                              let data16 = data.taskId;
                              const _errs73 = errors;
                              if (errors === _errs73) {
                                if (typeof data16 === "string") {
                                  if (func1(data16) < 1) {
                                    const err61 = {
                                      instancePath: instancePath + "/taskId",
                                      schemaPath:
                                        "#/$defs/TaskResource/properties/taskId/minLength",
                                      keyword: "minLength",
                                      params: { limit: 1 },
                                      message:
                                        "must NOT have fewer than 1 characters",
                                    };
                                    if (vErrors === null) {
                                      vErrors = [err61];
                                    } else {
                                      vErrors.push(err61);
                                    }
                                    errors++;
                                  }
                                } else {
                                  const err62 = {
                                    instancePath: instancePath + "/taskId",
                                    schemaPath:
                                      "#/$defs/TaskResource/properties/taskId/type",
                                    keyword: "type",
                                    params: { type: "string" },
                                    message: "must be string",
                                  };
                                  if (vErrors === null) {
                                    vErrors = [err62];
                                  } else {
                                    vErrors.push(err62);
                                  }
                                  errors++;
                                }
                              }
                              var valid20 = _errs73 === errors;
                            } else {
                              var valid20 = true;
                            }
                          }
                        }
                      }
                    } else {
                      const err63 = {
                        instancePath,
                        schemaPath: "#/$defs/TaskResource/type",
                        keyword: "type",
                        params: { type: "object" },
                        message: "must be object",
                      };
                      if (vErrors === null) {
                        vErrors = [err63];
                      } else {
                        vErrors.push(err63);
                      }
                      errors++;
                    }
                  }
                  var _valid0 = _errs67 === errors;
                  if (_valid0 && valid0) {
                    valid0 = false;
                    passing0 = [passing0, 9];
                  } else {
                    if (_valid0) {
                      valid0 = true;
                      passing0 = 9;
                      if (props0 !== true) {
                        props0 = true;
                      }
                    }
                    const _errs75 = errors;
                    const _errs76 = errors;
                    if (errors === _errs76) {
                      if (
                        data &&
                        typeof data == "object" &&
                        !Array.isArray(data)
                      ) {
                        let missing10;
                        if (
                          (data.kind === void 0 && (missing10 = "kind")) ||
                          (data.agentId === void 0 && (missing10 = "agentId"))
                        ) {
                          const err64 = {
                            instancePath,
                            schemaPath: "#/$defs/WorkspaceResource/required",
                            keyword: "required",
                            params: { missingProperty: missing10 },
                            message:
                              "must have required property '" + missing10 + "'",
                          };
                          if (vErrors === null) {
                            vErrors = [err64];
                          } else {
                            vErrors.push(err64);
                          }
                          errors++;
                        } else {
                          const _errs78 = errors;
                          for (const key10 in data) {
                            if (!(key10 === "kind" || key10 === "agentId")) {
                              const err65 = {
                                instancePath,
                                schemaPath:
                                  "#/$defs/WorkspaceResource/additionalProperties",
                                keyword: "additionalProperties",
                                params: { additionalProperty: key10 },
                                message: "must NOT have additional properties",
                              };
                              if (vErrors === null) {
                                vErrors = [err65];
                              } else {
                                vErrors.push(err65);
                              }
                              errors++;
                              break;
                            }
                          }
                          if (_errs78 === errors) {
                            if (data.kind !== void 0) {
                              let data17 = data.kind;
                              const _errs79 = errors;
                              if (typeof data17 !== "string") {
                                const err66 = {
                                  instancePath: instancePath + "/kind",
                                  schemaPath:
                                    "#/$defs/WorkspaceResource/properties/kind/type",
                                  keyword: "type",
                                  params: { type: "string" },
                                  message: "must be string",
                                };
                                if (vErrors === null) {
                                  vErrors = [err66];
                                } else {
                                  vErrors.push(err66);
                                }
                                errors++;
                              }
                              if ("workspace" !== data17) {
                                const err67 = {
                                  instancePath: instancePath + "/kind",
                                  schemaPath:
                                    "#/$defs/WorkspaceResource/properties/kind/const",
                                  keyword: "const",
                                  params: { allowedValue: "workspace" },
                                  message: "must be equal to constant",
                                };
                                if (vErrors === null) {
                                  vErrors = [err67];
                                } else {
                                  vErrors.push(err67);
                                }
                                errors++;
                              }
                              var valid22 = _errs79 === errors;
                            } else {
                              var valid22 = true;
                            }
                            if (valid22) {
                              if (data.agentId !== void 0) {
                                let data18 = data.agentId;
                                const _errs81 = errors;
                                if (errors === _errs81) {
                                  if (typeof data18 === "string") {
                                    if (func1(data18) < 1) {
                                      const err68 = {
                                        instancePath: instancePath + "/agentId",
                                        schemaPath:
                                          "#/$defs/WorkspaceResource/properties/agentId/minLength",
                                        keyword: "minLength",
                                        params: { limit: 1 },
                                        message:
                                          "must NOT have fewer than 1 characters",
                                      };
                                      if (vErrors === null) {
                                        vErrors = [err68];
                                      } else {
                                        vErrors.push(err68);
                                      }
                                      errors++;
                                    }
                                  } else {
                                    const err69 = {
                                      instancePath: instancePath + "/agentId",
                                      schemaPath:
                                        "#/$defs/WorkspaceResource/properties/agentId/type",
                                      keyword: "type",
                                      params: { type: "string" },
                                      message: "must be string",
                                    };
                                    if (vErrors === null) {
                                      vErrors = [err69];
                                    } else {
                                      vErrors.push(err69);
                                    }
                                    errors++;
                                  }
                                }
                                var valid22 = _errs81 === errors;
                              } else {
                                var valid22 = true;
                              }
                            }
                          }
                        }
                      } else {
                        const err70 = {
                          instancePath,
                          schemaPath: "#/$defs/WorkspaceResource/type",
                          keyword: "type",
                          params: { type: "object" },
                          message: "must be object",
                        };
                        if (vErrors === null) {
                          vErrors = [err70];
                        } else {
                          vErrors.push(err70);
                        }
                        errors++;
                      }
                    }
                    var _valid0 = _errs75 === errors;
                    if (_valid0 && valid0) {
                      valid0 = false;
                      passing0 = [passing0, 10];
                    } else {
                      if (_valid0) {
                        valid0 = true;
                        passing0 = 10;
                        if (props0 !== true) {
                          props0 = true;
                        }
                      }
                      const _errs83 = errors;
                      const _errs84 = errors;
                      if (errors === _errs84) {
                        if (
                          data &&
                          typeof data == "object" &&
                          !Array.isArray(data)
                        ) {
                          let missing11;
                          if (
                            (data.kind === void 0 && (missing11 = "kind")) ||
                            (data.agentId === void 0 && (missing11 = "agentId"))
                          ) {
                            const err71 = {
                              instancePath,
                              schemaPath: "#/$defs/VoiceResource/required",
                              keyword: "required",
                              params: { missingProperty: missing11 },
                              message:
                                "must have required property '" +
                                missing11 +
                                "'",
                            };
                            if (vErrors === null) {
                              vErrors = [err71];
                            } else {
                              vErrors.push(err71);
                            }
                            errors++;
                          } else {
                            const _errs86 = errors;
                            for (const key11 in data) {
                              if (!(key11 === "kind" || key11 === "agentId")) {
                                const err72 = {
                                  instancePath,
                                  schemaPath:
                                    "#/$defs/VoiceResource/additionalProperties",
                                  keyword: "additionalProperties",
                                  params: { additionalProperty: key11 },
                                  message:
                                    "must NOT have additional properties",
                                };
                                if (vErrors === null) {
                                  vErrors = [err72];
                                } else {
                                  vErrors.push(err72);
                                }
                                errors++;
                                break;
                              }
                            }
                            if (_errs86 === errors) {
                              if (data.kind !== void 0) {
                                let data19 = data.kind;
                                const _errs87 = errors;
                                if (typeof data19 !== "string") {
                                  const err73 = {
                                    instancePath: instancePath + "/kind",
                                    schemaPath:
                                      "#/$defs/VoiceResource/properties/kind/type",
                                    keyword: "type",
                                    params: { type: "string" },
                                    message: "must be string",
                                  };
                                  if (vErrors === null) {
                                    vErrors = [err73];
                                  } else {
                                    vErrors.push(err73);
                                  }
                                  errors++;
                                }
                                if ("voice" !== data19) {
                                  const err74 = {
                                    instancePath: instancePath + "/kind",
                                    schemaPath:
                                      "#/$defs/VoiceResource/properties/kind/const",
                                    keyword: "const",
                                    params: { allowedValue: "voice" },
                                    message: "must be equal to constant",
                                  };
                                  if (vErrors === null) {
                                    vErrors = [err74];
                                  } else {
                                    vErrors.push(err74);
                                  }
                                  errors++;
                                }
                                var valid24 = _errs87 === errors;
                              } else {
                                var valid24 = true;
                              }
                              if (valid24) {
                                if (data.agentId !== void 0) {
                                  let data20 = data.agentId;
                                  const _errs89 = errors;
                                  if (errors === _errs89) {
                                    if (typeof data20 === "string") {
                                      if (func1(data20) < 1) {
                                        const err75 = {
                                          instancePath:
                                            instancePath + "/agentId",
                                          schemaPath:
                                            "#/$defs/VoiceResource/properties/agentId/minLength",
                                          keyword: "minLength",
                                          params: { limit: 1 },
                                          message:
                                            "must NOT have fewer than 1 characters",
                                        };
                                        if (vErrors === null) {
                                          vErrors = [err75];
                                        } else {
                                          vErrors.push(err75);
                                        }
                                        errors++;
                                      }
                                    } else {
                                      const err76 = {
                                        instancePath: instancePath + "/agentId",
                                        schemaPath:
                                          "#/$defs/VoiceResource/properties/agentId/type",
                                        keyword: "type",
                                        params: { type: "string" },
                                        message: "must be string",
                                      };
                                      if (vErrors === null) {
                                        vErrors = [err76];
                                      } else {
                                        vErrors.push(err76);
                                      }
                                      errors++;
                                    }
                                  }
                                  var valid24 = _errs89 === errors;
                                } else {
                                  var valid24 = true;
                                }
                              }
                            }
                          }
                        } else {
                          const err77 = {
                            instancePath,
                            schemaPath: "#/$defs/VoiceResource/type",
                            keyword: "type",
                            params: { type: "object" },
                            message: "must be object",
                          };
                          if (vErrors === null) {
                            vErrors = [err77];
                          } else {
                            vErrors.push(err77);
                          }
                          errors++;
                        }
                      }
                      var _valid0 = _errs83 === errors;
                      if (_valid0 && valid0) {
                        valid0 = false;
                        passing0 = [passing0, 11];
                      } else {
                        if (_valid0) {
                          valid0 = true;
                          passing0 = 11;
                          if (props0 !== true) {
                            props0 = true;
                          }
                        }
                        const _errs91 = errors;
                        const _errs92 = errors;
                        if (errors === _errs92) {
                          if (
                            data &&
                            typeof data == "object" &&
                            !Array.isArray(data)
                          ) {
                            let missing12;
                            if (
                              (data.kind === void 0 && (missing12 = "kind")) ||
                              (data.agentId === void 0 &&
                                (missing12 = "agentId"))
                            ) {
                              const err78 = {
                                instancePath,
                                schemaPath:
                                  "#/$defs/SessionCostResource/required",
                                keyword: "required",
                                params: { missingProperty: missing12 },
                                message:
                                  "must have required property '" +
                                  missing12 +
                                  "'",
                              };
                              if (vErrors === null) {
                                vErrors = [err78];
                              } else {
                                vErrors.push(err78);
                              }
                              errors++;
                            } else {
                              const _errs94 = errors;
                              for (const key12 in data) {
                                if (
                                  !(key12 === "kind" || key12 === "agentId")
                                ) {
                                  const err79 = {
                                    instancePath,
                                    schemaPath:
                                      "#/$defs/SessionCostResource/additionalProperties",
                                    keyword: "additionalProperties",
                                    params: { additionalProperty: key12 },
                                    message:
                                      "must NOT have additional properties",
                                  };
                                  if (vErrors === null) {
                                    vErrors = [err79];
                                  } else {
                                    vErrors.push(err79);
                                  }
                                  errors++;
                                  break;
                                }
                              }
                              if (_errs94 === errors) {
                                if (data.kind !== void 0) {
                                  let data21 = data.kind;
                                  const _errs95 = errors;
                                  if (typeof data21 !== "string") {
                                    const err80 = {
                                      instancePath: instancePath + "/kind",
                                      schemaPath:
                                        "#/$defs/SessionCostResource/properties/kind/type",
                                      keyword: "type",
                                      params: { type: "string" },
                                      message: "must be string",
                                    };
                                    if (vErrors === null) {
                                      vErrors = [err80];
                                    } else {
                                      vErrors.push(err80);
                                    }
                                    errors++;
                                  }
                                  if ("session-cost" !== data21) {
                                    const err81 = {
                                      instancePath: instancePath + "/kind",
                                      schemaPath:
                                        "#/$defs/SessionCostResource/properties/kind/const",
                                      keyword: "const",
                                      params: { allowedValue: "session-cost" },
                                      message: "must be equal to constant",
                                    };
                                    if (vErrors === null) {
                                      vErrors = [err81];
                                    } else {
                                      vErrors.push(err81);
                                    }
                                    errors++;
                                  }
                                  var valid26 = _errs95 === errors;
                                } else {
                                  var valid26 = true;
                                }
                                if (valid26) {
                                  if (data.agentId !== void 0) {
                                    let data22 = data.agentId;
                                    const _errs97 = errors;
                                    if (errors === _errs97) {
                                      if (typeof data22 === "string") {
                                        if (func1(data22) < 1) {
                                          const err82 = {
                                            instancePath:
                                              instancePath + "/agentId",
                                            schemaPath:
                                              "#/$defs/SessionCostResource/properties/agentId/minLength",
                                            keyword: "minLength",
                                            params: { limit: 1 },
                                            message:
                                              "must NOT have fewer than 1 characters",
                                          };
                                          if (vErrors === null) {
                                            vErrors = [err82];
                                          } else {
                                            vErrors.push(err82);
                                          }
                                          errors++;
                                        }
                                      } else {
                                        const err83 = {
                                          instancePath:
                                            instancePath + "/agentId",
                                          schemaPath:
                                            "#/$defs/SessionCostResource/properties/agentId/type",
                                          keyword: "type",
                                          params: { type: "string" },
                                          message: "must be string",
                                        };
                                        if (vErrors === null) {
                                          vErrors = [err83];
                                        } else {
                                          vErrors.push(err83);
                                        }
                                        errors++;
                                      }
                                    }
                                    var valid26 = _errs97 === errors;
                                  } else {
                                    var valid26 = true;
                                  }
                                }
                              }
                            }
                          } else {
                            const err84 = {
                              instancePath,
                              schemaPath: "#/$defs/SessionCostResource/type",
                              keyword: "type",
                              params: { type: "object" },
                              message: "must be object",
                            };
                            if (vErrors === null) {
                              vErrors = [err84];
                            } else {
                              vErrors.push(err84);
                            }
                            errors++;
                          }
                        }
                        var _valid0 = _errs91 === errors;
                        if (_valid0 && valid0) {
                          valid0 = false;
                          passing0 = [passing0, 12];
                        } else {
                          if (_valid0) {
                            valid0 = true;
                            passing0 = 12;
                            if (props0 !== true) {
                              props0 = true;
                            }
                          }
                          const _errs99 = errors;
                          const _errs100 = errors;
                          if (errors === _errs100) {
                            if (
                              data &&
                              typeof data == "object" &&
                              !Array.isArray(data)
                            ) {
                              let missing13;
                              if (
                                data.kind === void 0 &&
                                (missing13 = "kind")
                              ) {
                                const err85 = {
                                  instancePath,
                                  schemaPath: "#/$defs/CostsResource/required",
                                  keyword: "required",
                                  params: { missingProperty: missing13 },
                                  message:
                                    "must have required property '" +
                                    missing13 +
                                    "'",
                                };
                                if (vErrors === null) {
                                  vErrors = [err85];
                                } else {
                                  vErrors.push(err85);
                                }
                                errors++;
                              } else {
                                const _errs102 = errors;
                                for (const key13 in data) {
                                  if (!(key13 === "kind")) {
                                    const err86 = {
                                      instancePath,
                                      schemaPath:
                                        "#/$defs/CostsResource/additionalProperties",
                                      keyword: "additionalProperties",
                                      params: { additionalProperty: key13 },
                                      message:
                                        "must NOT have additional properties",
                                    };
                                    if (vErrors === null) {
                                      vErrors = [err86];
                                    } else {
                                      vErrors.push(err86);
                                    }
                                    errors++;
                                    break;
                                  }
                                }
                                if (_errs102 === errors) {
                                  if (data.kind !== void 0) {
                                    let data23 = data.kind;
                                    if (typeof data23 !== "string") {
                                      const err87 = {
                                        instancePath: instancePath + "/kind",
                                        schemaPath:
                                          "#/$defs/CostsResource/properties/kind/type",
                                        keyword: "type",
                                        params: { type: "string" },
                                        message: "must be string",
                                      };
                                      if (vErrors === null) {
                                        vErrors = [err87];
                                      } else {
                                        vErrors.push(err87);
                                      }
                                      errors++;
                                    }
                                    if ("costs" !== data23) {
                                      const err88 = {
                                        instancePath: instancePath + "/kind",
                                        schemaPath:
                                          "#/$defs/CostsResource/properties/kind/const",
                                        keyword: "const",
                                        params: { allowedValue: "costs" },
                                        message: "must be equal to constant",
                                      };
                                      if (vErrors === null) {
                                        vErrors = [err88];
                                      } else {
                                        vErrors.push(err88);
                                      }
                                      errors++;
                                    }
                                  }
                                }
                              }
                            } else {
                              const err89 = {
                                instancePath,
                                schemaPath: "#/$defs/CostsResource/type",
                                keyword: "type",
                                params: { type: "object" },
                                message: "must be object",
                              };
                              if (vErrors === null) {
                                vErrors = [err89];
                              } else {
                                vErrors.push(err89);
                              }
                              errors++;
                            }
                          }
                          var _valid0 = _errs99 === errors;
                          if (_valid0 && valid0) {
                            valid0 = false;
                            passing0 = [passing0, 13];
                          } else {
                            if (_valid0) {
                              valid0 = true;
                              passing0 = 13;
                              if (props0 !== true) {
                                props0 = true;
                              }
                            }
                            const _errs105 = errors;
                            const _errs106 = errors;
                            if (errors === _errs106) {
                              if (
                                data &&
                                typeof data == "object" &&
                                !Array.isArray(data)
                              ) {
                                let missing14;
                                if (
                                  data.kind === void 0 &&
                                  (missing14 = "kind")
                                ) {
                                  const err90 = {
                                    instancePath,
                                    schemaPath:
                                      "#/$defs/DesktopResource/required",
                                    keyword: "required",
                                    params: { missingProperty: missing14 },
                                    message:
                                      "must have required property '" +
                                      missing14 +
                                      "'",
                                  };
                                  if (vErrors === null) {
                                    vErrors = [err90];
                                  } else {
                                    vErrors.push(err90);
                                  }
                                  errors++;
                                } else {
                                  const _errs108 = errors;
                                  for (const key14 in data) {
                                    if (!(key14 === "kind")) {
                                      const err91 = {
                                        instancePath,
                                        schemaPath:
                                          "#/$defs/DesktopResource/additionalProperties",
                                        keyword: "additionalProperties",
                                        params: { additionalProperty: key14 },
                                        message:
                                          "must NOT have additional properties",
                                      };
                                      if (vErrors === null) {
                                        vErrors = [err91];
                                      } else {
                                        vErrors.push(err91);
                                      }
                                      errors++;
                                      break;
                                    }
                                  }
                                  if (_errs108 === errors) {
                                    if (data.kind !== void 0) {
                                      let data24 = data.kind;
                                      if (typeof data24 !== "string") {
                                        const err92 = {
                                          instancePath: instancePath + "/kind",
                                          schemaPath:
                                            "#/$defs/DesktopResource/properties/kind/type",
                                          keyword: "type",
                                          params: { type: "string" },
                                          message: "must be string",
                                        };
                                        if (vErrors === null) {
                                          vErrors = [err92];
                                        } else {
                                          vErrors.push(err92);
                                        }
                                        errors++;
                                      }
                                      if ("desktop" !== data24) {
                                        const err93 = {
                                          instancePath: instancePath + "/kind",
                                          schemaPath:
                                            "#/$defs/DesktopResource/properties/kind/const",
                                          keyword: "const",
                                          params: { allowedValue: "desktop" },
                                          message: "must be equal to constant",
                                        };
                                        if (vErrors === null) {
                                          vErrors = [err93];
                                        } else {
                                          vErrors.push(err93);
                                        }
                                        errors++;
                                      }
                                    }
                                  }
                                }
                              } else {
                                const err94 = {
                                  instancePath,
                                  schemaPath: "#/$defs/DesktopResource/type",
                                  keyword: "type",
                                  params: { type: "object" },
                                  message: "must be object",
                                };
                                if (vErrors === null) {
                                  vErrors = [err94];
                                } else {
                                  vErrors.push(err94);
                                }
                                errors++;
                              }
                            }
                            var _valid0 = _errs105 === errors;
                            if (_valid0 && valid0) {
                              valid0 = false;
                              passing0 = [passing0, 14];
                            } else {
                              if (_valid0) {
                                valid0 = true;
                                passing0 = 14;
                                if (props0 !== true) {
                                  props0 = true;
                                }
                              }
                              const _errs111 = errors;
                              const _errs112 = errors;
                              if (errors === _errs112) {
                                if (
                                  data &&
                                  typeof data == "object" &&
                                  !Array.isArray(data)
                                ) {
                                  let missing15;
                                  if (
                                    (data.kind === void 0 &&
                                      (missing15 = "kind")) ||
                                    (data.agentId === void 0 &&
                                      (missing15 = "agentId"))
                                  ) {
                                    const err95 = {
                                      instancePath,
                                      schemaPath:
                                        "#/$defs/WorktreeDiskResource/required",
                                      keyword: "required",
                                      params: { missingProperty: missing15 },
                                      message:
                                        "must have required property '" +
                                        missing15 +
                                        "'",
                                    };
                                    if (vErrors === null) {
                                      vErrors = [err95];
                                    } else {
                                      vErrors.push(err95);
                                    }
                                    errors++;
                                  } else {
                                    const _errs114 = errors;
                                    for (const key15 in data) {
                                      if (
                                        !(
                                          key15 === "kind" ||
                                          key15 === "agentId"
                                        )
                                      ) {
                                        const err96 = {
                                          instancePath,
                                          schemaPath:
                                            "#/$defs/WorktreeDiskResource/additionalProperties",
                                          keyword: "additionalProperties",
                                          params: { additionalProperty: key15 },
                                          message:
                                            "must NOT have additional properties",
                                        };
                                        if (vErrors === null) {
                                          vErrors = [err96];
                                        } else {
                                          vErrors.push(err96);
                                        }
                                        errors++;
                                        break;
                                      }
                                    }
                                    if (_errs114 === errors) {
                                      if (data.kind !== void 0) {
                                        let data25 = data.kind;
                                        const _errs115 = errors;
                                        if (typeof data25 !== "string") {
                                          const err97 = {
                                            instancePath:
                                              instancePath + "/kind",
                                            schemaPath:
                                              "#/$defs/WorktreeDiskResource/properties/kind/type",
                                            keyword: "type",
                                            params: { type: "string" },
                                            message: "must be string",
                                          };
                                          if (vErrors === null) {
                                            vErrors = [err97];
                                          } else {
                                            vErrors.push(err97);
                                          }
                                          errors++;
                                        }
                                        if ("worktree-disk" !== data25) {
                                          const err98 = {
                                            instancePath:
                                              instancePath + "/kind",
                                            schemaPath:
                                              "#/$defs/WorktreeDiskResource/properties/kind/const",
                                            keyword: "const",
                                            params: {
                                              allowedValue: "worktree-disk",
                                            },
                                            message:
                                              "must be equal to constant",
                                          };
                                          if (vErrors === null) {
                                            vErrors = [err98];
                                          } else {
                                            vErrors.push(err98);
                                          }
                                          errors++;
                                        }
                                        var valid32 = _errs115 === errors;
                                      } else {
                                        var valid32 = true;
                                      }
                                      if (valid32) {
                                        if (data.agentId !== void 0) {
                                          let data26 = data.agentId;
                                          const _errs117 = errors;
                                          if (errors === _errs117) {
                                            if (typeof data26 === "string") {
                                              if (func1(data26) < 1) {
                                                const err99 = {
                                                  instancePath:
                                                    instancePath + "/agentId",
                                                  schemaPath:
                                                    "#/$defs/WorktreeDiskResource/properties/agentId/minLength",
                                                  keyword: "minLength",
                                                  params: { limit: 1 },
                                                  message:
                                                    "must NOT have fewer than 1 characters",
                                                };
                                                if (vErrors === null) {
                                                  vErrors = [err99];
                                                } else {
                                                  vErrors.push(err99);
                                                }
                                                errors++;
                                              }
                                            } else {
                                              const err100 = {
                                                instancePath:
                                                  instancePath + "/agentId",
                                                schemaPath:
                                                  "#/$defs/WorktreeDiskResource/properties/agentId/type",
                                                keyword: "type",
                                                params: { type: "string" },
                                                message: "must be string",
                                              };
                                              if (vErrors === null) {
                                                vErrors = [err100];
                                              } else {
                                                vErrors.push(err100);
                                              }
                                              errors++;
                                            }
                                          }
                                          var valid32 = _errs117 === errors;
                                        } else {
                                          var valid32 = true;
                                        }
                                      }
                                    }
                                  }
                                } else {
                                  const err101 = {
                                    instancePath,
                                    schemaPath:
                                      "#/$defs/WorktreeDiskResource/type",
                                    keyword: "type",
                                    params: { type: "object" },
                                    message: "must be object",
                                  };
                                  if (vErrors === null) {
                                    vErrors = [err101];
                                  } else {
                                    vErrors.push(err101);
                                  }
                                  errors++;
                                }
                              }
                              var _valid0 = _errs111 === errors;
                              if (_valid0 && valid0) {
                                valid0 = false;
                                passing0 = [passing0, 15];
                              } else {
                                if (_valid0) {
                                  valid0 = true;
                                  passing0 = 15;
                                  if (props0 !== true) {
                                    props0 = true;
                                  }
                                }
                                const _errs119 = errors;
                                const _errs120 = errors;
                                if (errors === _errs120) {
                                  if (
                                    data &&
                                    typeof data == "object" &&
                                    !Array.isArray(data)
                                  ) {
                                    let missing16;
                                    if (
                                      (data.kind === void 0 &&
                                        (missing16 = "kind")) ||
                                      (data.roomId === void 0 &&
                                        (missing16 = "roomId"))
                                    ) {
                                      const err102 = {
                                        instancePath,
                                        schemaPath:
                                          "#/$defs/RoomResource/required",
                                        keyword: "required",
                                        params: { missingProperty: missing16 },
                                        message:
                                          "must have required property '" +
                                          missing16 +
                                          "'",
                                      };
                                      if (vErrors === null) {
                                        vErrors = [err102];
                                      } else {
                                        vErrors.push(err102);
                                      }
                                      errors++;
                                    } else {
                                      const _errs122 = errors;
                                      for (const key16 in data) {
                                        if (
                                          !(
                                            key16 === "kind" ||
                                            key16 === "roomId"
                                          )
                                        ) {
                                          const err103 = {
                                            instancePath,
                                            schemaPath:
                                              "#/$defs/RoomResource/additionalProperties",
                                            keyword: "additionalProperties",
                                            params: {
                                              additionalProperty: key16,
                                            },
                                            message:
                                              "must NOT have additional properties",
                                          };
                                          if (vErrors === null) {
                                            vErrors = [err103];
                                          } else {
                                            vErrors.push(err103);
                                          }
                                          errors++;
                                          break;
                                        }
                                      }
                                      if (_errs122 === errors) {
                                        if (data.kind !== void 0) {
                                          let data27 = data.kind;
                                          const _errs123 = errors;
                                          if (typeof data27 !== "string") {
                                            const err104 = {
                                              instancePath:
                                                instancePath + "/kind",
                                              schemaPath:
                                                "#/$defs/RoomResource/properties/kind/type",
                                              keyword: "type",
                                              params: { type: "string" },
                                              message: "must be string",
                                            };
                                            if (vErrors === null) {
                                              vErrors = [err104];
                                            } else {
                                              vErrors.push(err104);
                                            }
                                            errors++;
                                          }
                                          if ("room" !== data27) {
                                            const err105 = {
                                              instancePath:
                                                instancePath + "/kind",
                                              schemaPath:
                                                "#/$defs/RoomResource/properties/kind/const",
                                              keyword: "const",
                                              params: { allowedValue: "room" },
                                              message:
                                                "must be equal to constant",
                                            };
                                            if (vErrors === null) {
                                              vErrors = [err105];
                                            } else {
                                              vErrors.push(err105);
                                            }
                                            errors++;
                                          }
                                          var valid34 = _errs123 === errors;
                                        } else {
                                          var valid34 = true;
                                        }
                                        if (valid34) {
                                          if (data.roomId !== void 0) {
                                            let data28 = data.roomId;
                                            const _errs125 = errors;
                                            if (errors === _errs125) {
                                              if (typeof data28 === "string") {
                                                if (func1(data28) < 1) {
                                                  const err106 = {
                                                    instancePath:
                                                      instancePath + "/roomId",
                                                    schemaPath:
                                                      "#/$defs/RoomResource/properties/roomId/minLength",
                                                    keyword: "minLength",
                                                    params: { limit: 1 },
                                                    message:
                                                      "must NOT have fewer than 1 characters",
                                                  };
                                                  if (vErrors === null) {
                                                    vErrors = [err106];
                                                  } else {
                                                    vErrors.push(err106);
                                                  }
                                                  errors++;
                                                }
                                              } else {
                                                const err107 = {
                                                  instancePath:
                                                    instancePath + "/roomId",
                                                  schemaPath:
                                                    "#/$defs/RoomResource/properties/roomId/type",
                                                  keyword: "type",
                                                  params: { type: "string" },
                                                  message: "must be string",
                                                };
                                                if (vErrors === null) {
                                                  vErrors = [err107];
                                                } else {
                                                  vErrors.push(err107);
                                                }
                                                errors++;
                                              }
                                            }
                                            var valid34 = _errs125 === errors;
                                          } else {
                                            var valid34 = true;
                                          }
                                        }
                                      }
                                    }
                                  } else {
                                    const err108 = {
                                      instancePath,
                                      schemaPath: "#/$defs/RoomResource/type",
                                      keyword: "type",
                                      params: { type: "object" },
                                      message: "must be object",
                                    };
                                    if (vErrors === null) {
                                      vErrors = [err108];
                                    } else {
                                      vErrors.push(err108);
                                    }
                                    errors++;
                                  }
                                }
                                var _valid0 = _errs119 === errors;
                                if (_valid0 && valid0) {
                                  valid0 = false;
                                  passing0 = [passing0, 16];
                                } else {
                                  if (_valid0) {
                                    valid0 = true;
                                    passing0 = 16;
                                    if (props0 !== true) {
                                      props0 = true;
                                    }
                                  }
                                  const _errs127 = errors;
                                  const _errs128 = errors;
                                  if (errors === _errs128) {
                                    if (
                                      data &&
                                      typeof data == "object" &&
                                      !Array.isArray(data)
                                    ) {
                                      let missing17;
                                      if (
                                        data.kind === void 0 &&
                                        (missing17 = "kind")
                                      ) {
                                        const err109 = {
                                          instancePath,
                                          schemaPath:
                                            "#/$defs/StateResource/required",
                                          keyword: "required",
                                          params: {
                                            missingProperty: missing17,
                                          },
                                          message:
                                            "must have required property '" +
                                            missing17 +
                                            "'",
                                        };
                                        if (vErrors === null) {
                                          vErrors = [err109];
                                        } else {
                                          vErrors.push(err109);
                                        }
                                        errors++;
                                      } else {
                                        const _errs130 = errors;
                                        for (const key17 in data) {
                                          if (!(key17 === "kind")) {
                                            const err110 = {
                                              instancePath,
                                              schemaPath:
                                                "#/$defs/StateResource/additionalProperties",
                                              keyword: "additionalProperties",
                                              params: {
                                                additionalProperty: key17,
                                              },
                                              message:
                                                "must NOT have additional properties",
                                            };
                                            if (vErrors === null) {
                                              vErrors = [err110];
                                            } else {
                                              vErrors.push(err110);
                                            }
                                            errors++;
                                            break;
                                          }
                                        }
                                        if (_errs130 === errors) {
                                          if (data.kind !== void 0) {
                                            let data29 = data.kind;
                                            if (typeof data29 !== "string") {
                                              const err111 = {
                                                instancePath:
                                                  instancePath + "/kind",
                                                schemaPath:
                                                  "#/$defs/StateResource/properties/kind/type",
                                                keyword: "type",
                                                params: { type: "string" },
                                                message: "must be string",
                                              };
                                              if (vErrors === null) {
                                                vErrors = [err111];
                                              } else {
                                                vErrors.push(err111);
                                              }
                                              errors++;
                                            }
                                            if ("state" !== data29) {
                                              const err112 = {
                                                instancePath:
                                                  instancePath + "/kind",
                                                schemaPath:
                                                  "#/$defs/StateResource/properties/kind/const",
                                                keyword: "const",
                                                params: {
                                                  allowedValue: "state",
                                                },
                                                message:
                                                  "must be equal to constant",
                                              };
                                              if (vErrors === null) {
                                                vErrors = [err112];
                                              } else {
                                                vErrors.push(err112);
                                              }
                                              errors++;
                                            }
                                          }
                                        }
                                      }
                                    } else {
                                      const err113 = {
                                        instancePath,
                                        schemaPath:
                                          "#/$defs/StateResource/type",
                                        keyword: "type",
                                        params: { type: "object" },
                                        message: "must be object",
                                      };
                                      if (vErrors === null) {
                                        vErrors = [err113];
                                      } else {
                                        vErrors.push(err113);
                                      }
                                      errors++;
                                    }
                                  }
                                  var _valid0 = _errs127 === errors;
                                  if (_valid0 && valid0) {
                                    valid0 = false;
                                    passing0 = [passing0, 17];
                                  } else {
                                    if (_valid0) {
                                      valid0 = true;
                                      passing0 = 17;
                                      if (props0 !== true) {
                                        props0 = true;
                                      }
                                    }
                                    const _errs133 = errors;
                                    const _errs134 = errors;
                                    if (errors === _errs134) {
                                      if (
                                        data &&
                                        typeof data == "object" &&
                                        !Array.isArray(data)
                                      ) {
                                        let missing18;
                                        if (
                                          data.kind === void 0 &&
                                          (missing18 = "kind")
                                        ) {
                                          const err114 = {
                                            instancePath,
                                            schemaPath:
                                              "#/$defs/DraftsResource/required",
                                            keyword: "required",
                                            params: {
                                              missingProperty: missing18,
                                            },
                                            message:
                                              "must have required property '" +
                                              missing18 +
                                              "'",
                                          };
                                          if (vErrors === null) {
                                            vErrors = [err114];
                                          } else {
                                            vErrors.push(err114);
                                          }
                                          errors++;
                                        } else {
                                          const _errs136 = errors;
                                          for (const key18 in data) {
                                            if (!(key18 === "kind")) {
                                              const err115 = {
                                                instancePath,
                                                schemaPath:
                                                  "#/$defs/DraftsResource/additionalProperties",
                                                keyword: "additionalProperties",
                                                params: {
                                                  additionalProperty: key18,
                                                },
                                                message:
                                                  "must NOT have additional properties",
                                              };
                                              if (vErrors === null) {
                                                vErrors = [err115];
                                              } else {
                                                vErrors.push(err115);
                                              }
                                              errors++;
                                              break;
                                            }
                                          }
                                          if (_errs136 === errors) {
                                            if (data.kind !== void 0) {
                                              let data30 = data.kind;
                                              if (typeof data30 !== "string") {
                                                const err116 = {
                                                  instancePath:
                                                    instancePath + "/kind",
                                                  schemaPath:
                                                    "#/$defs/DraftsResource/properties/kind/type",
                                                  keyword: "type",
                                                  params: { type: "string" },
                                                  message: "must be string",
                                                };
                                                if (vErrors === null) {
                                                  vErrors = [err116];
                                                } else {
                                                  vErrors.push(err116);
                                                }
                                                errors++;
                                              }
                                              if ("drafts" !== data30) {
                                                const err117 = {
                                                  instancePath:
                                                    instancePath + "/kind",
                                                  schemaPath:
                                                    "#/$defs/DraftsResource/properties/kind/const",
                                                  keyword: "const",
                                                  params: {
                                                    allowedValue: "drafts",
                                                  },
                                                  message:
                                                    "must be equal to constant",
                                                };
                                                if (vErrors === null) {
                                                  vErrors = [err117];
                                                } else {
                                                  vErrors.push(err117);
                                                }
                                                errors++;
                                              }
                                            }
                                          }
                                        }
                                      } else {
                                        const err118 = {
                                          instancePath,
                                          schemaPath:
                                            "#/$defs/DraftsResource/type",
                                          keyword: "type",
                                          params: { type: "object" },
                                          message: "must be object",
                                        };
                                        if (vErrors === null) {
                                          vErrors = [err118];
                                        } else {
                                          vErrors.push(err118);
                                        }
                                        errors++;
                                      }
                                    }
                                    var _valid0 = _errs133 === errors;
                                    if (_valid0 && valid0) {
                                      valid0 = false;
                                      passing0 = [passing0, 18];
                                    } else {
                                      if (_valid0) {
                                        valid0 = true;
                                        passing0 = 18;
                                        if (props0 !== true) {
                                          props0 = true;
                                        }
                                      }
                                      const _errs139 = errors;
                                      const _errs140 = errors;
                                      if (errors === _errs140) {
                                        if (
                                          data &&
                                          typeof data == "object" &&
                                          !Array.isArray(data)
                                        ) {
                                          let missing19;
                                          if (
                                            data.kind === void 0 &&
                                            (missing19 = "kind")
                                          ) {
                                            const err119 = {
                                              instancePath,
                                              schemaPath:
                                                "#/$defs/TranscriptsResource/required",
                                              keyword: "required",
                                              params: {
                                                missingProperty: missing19,
                                              },
                                              message:
                                                "must have required property '" +
                                                missing19 +
                                                "'",
                                            };
                                            if (vErrors === null) {
                                              vErrors = [err119];
                                            } else {
                                              vErrors.push(err119);
                                            }
                                            errors++;
                                          } else {
                                            const _errs142 = errors;
                                            for (const key19 in data) {
                                              if (!(key19 === "kind")) {
                                                const err120 = {
                                                  instancePath,
                                                  schemaPath:
                                                    "#/$defs/TranscriptsResource/additionalProperties",
                                                  keyword:
                                                    "additionalProperties",
                                                  params: {
                                                    additionalProperty: key19,
                                                  },
                                                  message:
                                                    "must NOT have additional properties",
                                                };
                                                if (vErrors === null) {
                                                  vErrors = [err120];
                                                } else {
                                                  vErrors.push(err120);
                                                }
                                                errors++;
                                                break;
                                              }
                                            }
                                            if (_errs142 === errors) {
                                              if (data.kind !== void 0) {
                                                let data31 = data.kind;
                                                if (
                                                  typeof data31 !== "string"
                                                ) {
                                                  const err121 = {
                                                    instancePath:
                                                      instancePath + "/kind",
                                                    schemaPath:
                                                      "#/$defs/TranscriptsResource/properties/kind/type",
                                                    keyword: "type",
                                                    params: { type: "string" },
                                                    message: "must be string",
                                                  };
                                                  if (vErrors === null) {
                                                    vErrors = [err121];
                                                  } else {
                                                    vErrors.push(err121);
                                                  }
                                                  errors++;
                                                }
                                                if ("transcripts" !== data31) {
                                                  const err122 = {
                                                    instancePath:
                                                      instancePath + "/kind",
                                                    schemaPath:
                                                      "#/$defs/TranscriptsResource/properties/kind/const",
                                                    keyword: "const",
                                                    params: {
                                                      allowedValue:
                                                        "transcripts",
                                                    },
                                                    message:
                                                      "must be equal to constant",
                                                  };
                                                  if (vErrors === null) {
                                                    vErrors = [err122];
                                                  } else {
                                                    vErrors.push(err122);
                                                  }
                                                  errors++;
                                                }
                                              }
                                            }
                                          }
                                        } else {
                                          const err123 = {
                                            instancePath,
                                            schemaPath:
                                              "#/$defs/TranscriptsResource/type",
                                            keyword: "type",
                                            params: { type: "object" },
                                            message: "must be object",
                                          };
                                          if (vErrors === null) {
                                            vErrors = [err123];
                                          } else {
                                            vErrors.push(err123);
                                          }
                                          errors++;
                                        }
                                      }
                                      var _valid0 = _errs139 === errors;
                                      if (_valid0 && valid0) {
                                        valid0 = false;
                                        passing0 = [passing0, 19];
                                      } else {
                                        if (_valid0) {
                                          valid0 = true;
                                          passing0 = 19;
                                          if (props0 !== true) {
                                            props0 = true;
                                          }
                                        }
                                        const _errs145 = errors;
                                        const _errs146 = errors;
                                        if (errors === _errs146) {
                                          if (
                                            data &&
                                            typeof data == "object" &&
                                            !Array.isArray(data)
                                          ) {
                                            let missing20;
                                            if (
                                              (data.kind === void 0 &&
                                                (missing20 = "kind")) ||
                                              (data.agentId === void 0 &&
                                                (missing20 = "agentId"))
                                            ) {
                                              const err124 = {
                                                instancePath,
                                                schemaPath:
                                                  "#/$defs/TranscriptResource/required",
                                                keyword: "required",
                                                params: {
                                                  missingProperty: missing20,
                                                },
                                                message:
                                                  "must have required property '" +
                                                  missing20 +
                                                  "'",
                                              };
                                              if (vErrors === null) {
                                                vErrors = [err124];
                                              } else {
                                                vErrors.push(err124);
                                              }
                                              errors++;
                                            } else {
                                              const _errs148 = errors;
                                              for (const key20 in data) {
                                                if (
                                                  !(
                                                    key20 === "kind" ||
                                                    key20 === "agentId"
                                                  )
                                                ) {
                                                  const err125 = {
                                                    instancePath,
                                                    schemaPath:
                                                      "#/$defs/TranscriptResource/additionalProperties",
                                                    keyword:
                                                      "additionalProperties",
                                                    params: {
                                                      additionalProperty: key20,
                                                    },
                                                    message:
                                                      "must NOT have additional properties",
                                                  };
                                                  if (vErrors === null) {
                                                    vErrors = [err125];
                                                  } else {
                                                    vErrors.push(err125);
                                                  }
                                                  errors++;
                                                  break;
                                                }
                                              }
                                              if (_errs148 === errors) {
                                                if (data.kind !== void 0) {
                                                  let data32 = data.kind;
                                                  const _errs149 = errors;
                                                  if (
                                                    typeof data32 !== "string"
                                                  ) {
                                                    const err126 = {
                                                      instancePath:
                                                        instancePath + "/kind",
                                                      schemaPath:
                                                        "#/$defs/TranscriptResource/properties/kind/type",
                                                      keyword: "type",
                                                      params: {
                                                        type: "string",
                                                      },
                                                      message: "must be string",
                                                    };
                                                    if (vErrors === null) {
                                                      vErrors = [err126];
                                                    } else {
                                                      vErrors.push(err126);
                                                    }
                                                    errors++;
                                                  }
                                                  if ("transcript" !== data32) {
                                                    const err127 = {
                                                      instancePath:
                                                        instancePath + "/kind",
                                                      schemaPath:
                                                        "#/$defs/TranscriptResource/properties/kind/const",
                                                      keyword: "const",
                                                      params: {
                                                        allowedValue:
                                                          "transcript",
                                                      },
                                                      message:
                                                        "must be equal to constant",
                                                    };
                                                    if (vErrors === null) {
                                                      vErrors = [err127];
                                                    } else {
                                                      vErrors.push(err127);
                                                    }
                                                    errors++;
                                                  }
                                                  var valid42 =
                                                    _errs149 === errors;
                                                } else {
                                                  var valid42 = true;
                                                }
                                                if (valid42) {
                                                  if (data.agentId !== void 0) {
                                                    let data33 = data.agentId;
                                                    const _errs151 = errors;
                                                    if (errors === _errs151) {
                                                      if (
                                                        typeof data33 ===
                                                        "string"
                                                      ) {
                                                        if (func1(data33) < 1) {
                                                          const err128 = {
                                                            instancePath:
                                                              instancePath +
                                                              "/agentId",
                                                            schemaPath:
                                                              "#/$defs/TranscriptResource/properties/agentId/minLength",
                                                            keyword:
                                                              "minLength",
                                                            params: {
                                                              limit: 1,
                                                            },
                                                            message:
                                                              "must NOT have fewer than 1 characters",
                                                          };
                                                          if (
                                                            vErrors === null
                                                          ) {
                                                            vErrors = [err128];
                                                          } else {
                                                            vErrors.push(
                                                              err128,
                                                            );
                                                          }
                                                          errors++;
                                                        }
                                                      } else {
                                                        const err129 = {
                                                          instancePath:
                                                            instancePath +
                                                            "/agentId",
                                                          schemaPath:
                                                            "#/$defs/TranscriptResource/properties/agentId/type",
                                                          keyword: "type",
                                                          params: {
                                                            type: "string",
                                                          },
                                                          message:
                                                            "must be string",
                                                        };
                                                        if (vErrors === null) {
                                                          vErrors = [err129];
                                                        } else {
                                                          vErrors.push(err129);
                                                        }
                                                        errors++;
                                                      }
                                                    }
                                                    var valid42 =
                                                      _errs151 === errors;
                                                  } else {
                                                    var valid42 = true;
                                                  }
                                                }
                                              }
                                            }
                                          } else {
                                            const err130 = {
                                              instancePath,
                                              schemaPath:
                                                "#/$defs/TranscriptResource/type",
                                              keyword: "type",
                                              params: { type: "object" },
                                              message: "must be object",
                                            };
                                            if (vErrors === null) {
                                              vErrors = [err130];
                                            } else {
                                              vErrors.push(err130);
                                            }
                                            errors++;
                                          }
                                        }
                                        var _valid0 = _errs145 === errors;
                                        if (_valid0 && valid0) {
                                          valid0 = false;
                                          passing0 = [passing0, 20];
                                        } else {
                                          if (_valid0) {
                                            valid0 = true;
                                            passing0 = 20;
                                            if (props0 !== true) {
                                              props0 = true;
                                            }
                                          }
                                        }
                                      }
                                    }
                                  }
                                }
                              }
                            }
                          }
                        }
                      }
                    }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
  if (!valid0) {
    const err131 = {
      instancePath,
      schemaPath: "#/oneOf",
      keyword: "oneOf",
      params: { passingSchemas: passing0 },
      message: "must match exactly one schema in oneOf",
    };
    if (vErrors === null) {
      vErrors = [err131];
    } else {
      vErrors.push(err131);
    }
    errors++;
    validate21.errors = vErrors;
    return false;
  } else {
    errors = _errs0;
    if (vErrors !== null) {
      if (_errs0) {
        vErrors.length = _errs0;
      } else {
        vErrors = null;
      }
    }
  }
  validate21.errors = vErrors;
  evaluated0.props = props0;
  return errors === 0;
}
validate21.evaluated = { dynamicProps: true, dynamicItems: false };
var isResourceChangeEvent = validate22;
var schema54 = {
  additionalProperties: false,
  description:
    "Named `resources` SSE payload. A change invalidates the listed refs.",
  properties: {
    protocol: { const: 3, title: "Protocol", type: "integer" },
    workspaceId: { minLength: 1, title: "Workspaceid", type: "string" },
    epoch: { minLength: 1, title: "Epoch", type: "string" },
    revision: {
      maximum: 9007199254740991,
      minimum: 0,
      title: "Revision",
      type: "integer",
    },
    reason: {
      enum: ["initial", "change", "reconnect", "overflow", "workspace"],
      title: "Reason",
      type: "string",
    },
    resources: {
      items: { $ref: "#/$defs/ResourceRef" },
      title: "Resources",
      type: "array",
    },
  },
  required: [
    "protocol",
    "workspaceId",
    "epoch",
    "revision",
    "reason",
    "resources",
  ],
  title: "ResourceChangeEvent",
  type: "object",
};
function validate22(
  data,
  {
    instancePath = "",
    parentData,
    parentDataProperty,
    rootData = data,
    dynamicAnchors = {},
  } = {},
) {
  let vErrors = null;
  let errors = 0;
  const evaluated0 = validate22.evaluated;
  if (evaluated0.dynamicProps) {
    evaluated0.props = void 0;
  }
  if (evaluated0.dynamicItems) {
    evaluated0.items = void 0;
  }
  if (errors === 0) {
    if (data && typeof data == "object" && !Array.isArray(data)) {
      let missing0;
      if (
        (data.protocol === void 0 && (missing0 = "protocol")) ||
        (data.workspaceId === void 0 && (missing0 = "workspaceId")) ||
        (data.epoch === void 0 && (missing0 = "epoch")) ||
        (data.revision === void 0 && (missing0 = "revision")) ||
        (data.reason === void 0 && (missing0 = "reason")) ||
        (data.resources === void 0 && (missing0 = "resources"))
      ) {
        validate22.errors = [
          {
            instancePath,
            schemaPath: "#/required",
            keyword: "required",
            params: { missingProperty: missing0 },
            message: "must have required property '" + missing0 + "'",
          },
        ];
        return false;
      } else {
        const _errs1 = errors;
        for (const key0 in data) {
          if (
            !(
              key0 === "protocol" ||
              key0 === "workspaceId" ||
              key0 === "epoch" ||
              key0 === "revision" ||
              key0 === "reason" ||
              key0 === "resources"
            )
          ) {
            validate22.errors = [
              {
                instancePath,
                schemaPath: "#/additionalProperties",
                keyword: "additionalProperties",
                params: { additionalProperty: key0 },
                message: "must NOT have additional properties",
              },
            ];
            return false;
            break;
          }
        }
        if (_errs1 === errors) {
          if (data.protocol !== void 0) {
            let data0 = data.protocol;
            const _errs2 = errors;
            if (
              !(
                typeof data0 == "number" &&
                !(data0 % 1) &&
                !isNaN(data0) &&
                isFinite(data0)
              )
            ) {
              validate22.errors = [
                {
                  instancePath: instancePath + "/protocol",
                  schemaPath: "#/properties/protocol/type",
                  keyword: "type",
                  params: { type: "integer" },
                  message: "must be integer",
                },
              ];
              return false;
            }
            if (3 !== data0) {
              validate22.errors = [
                {
                  instancePath: instancePath + "/protocol",
                  schemaPath: "#/properties/protocol/const",
                  keyword: "const",
                  params: { allowedValue: 3 },
                  message: "must be equal to constant",
                },
              ];
              return false;
            }
            var valid0 = _errs2 === errors;
          } else {
            var valid0 = true;
          }
          if (valid0) {
            if (data.workspaceId !== void 0) {
              let data1 = data.workspaceId;
              const _errs4 = errors;
              if (errors === _errs4) {
                if (typeof data1 === "string") {
                  if (func1(data1) < 1) {
                    validate22.errors = [
                      {
                        instancePath: instancePath + "/workspaceId",
                        schemaPath: "#/properties/workspaceId/minLength",
                        keyword: "minLength",
                        params: { limit: 1 },
                        message: "must NOT have fewer than 1 characters",
                      },
                    ];
                    return false;
                  }
                } else {
                  validate22.errors = [
                    {
                      instancePath: instancePath + "/workspaceId",
                      schemaPath: "#/properties/workspaceId/type",
                      keyword: "type",
                      params: { type: "string" },
                      message: "must be string",
                    },
                  ];
                  return false;
                }
              }
              var valid0 = _errs4 === errors;
            } else {
              var valid0 = true;
            }
            if (valid0) {
              if (data.epoch !== void 0) {
                let data2 = data.epoch;
                const _errs6 = errors;
                if (errors === _errs6) {
                  if (typeof data2 === "string") {
                    if (func1(data2) < 1) {
                      validate22.errors = [
                        {
                          instancePath: instancePath + "/epoch",
                          schemaPath: "#/properties/epoch/minLength",
                          keyword: "minLength",
                          params: { limit: 1 },
                          message: "must NOT have fewer than 1 characters",
                        },
                      ];
                      return false;
                    }
                  } else {
                    validate22.errors = [
                      {
                        instancePath: instancePath + "/epoch",
                        schemaPath: "#/properties/epoch/type",
                        keyword: "type",
                        params: { type: "string" },
                        message: "must be string",
                      },
                    ];
                    return false;
                  }
                }
                var valid0 = _errs6 === errors;
              } else {
                var valid0 = true;
              }
              if (valid0) {
                if (data.revision !== void 0) {
                  let data3 = data.revision;
                  const _errs8 = errors;
                  if (
                    !(
                      typeof data3 == "number" &&
                      !(data3 % 1) &&
                      !isNaN(data3) &&
                      isFinite(data3)
                    )
                  ) {
                    validate22.errors = [
                      {
                        instancePath: instancePath + "/revision",
                        schemaPath: "#/properties/revision/type",
                        keyword: "type",
                        params: { type: "integer" },
                        message: "must be integer",
                      },
                    ];
                    return false;
                  }
                  if (errors === _errs8) {
                    if (typeof data3 == "number" && isFinite(data3)) {
                      if (data3 > 9007199254740991 || isNaN(data3)) {
                        validate22.errors = [
                          {
                            instancePath: instancePath + "/revision",
                            schemaPath: "#/properties/revision/maximum",
                            keyword: "maximum",
                            params: {
                              comparison: "<=",
                              limit: 9007199254740991,
                            },
                            message: "must be <= 9007199254740991",
                          },
                        ];
                        return false;
                      } else {
                        if (data3 < 0 || isNaN(data3)) {
                          validate22.errors = [
                            {
                              instancePath: instancePath + "/revision",
                              schemaPath: "#/properties/revision/minimum",
                              keyword: "minimum",
                              params: { comparison: ">=", limit: 0 },
                              message: "must be >= 0",
                            },
                          ];
                          return false;
                        }
                      }
                    }
                  }
                  var valid0 = _errs8 === errors;
                } else {
                  var valid0 = true;
                }
                if (valid0) {
                  if (data.reason !== void 0) {
                    let data4 = data.reason;
                    const _errs10 = errors;
                    if (typeof data4 !== "string") {
                      validate22.errors = [
                        {
                          instancePath: instancePath + "/reason",
                          schemaPath: "#/properties/reason/type",
                          keyword: "type",
                          params: { type: "string" },
                          message: "must be string",
                        },
                      ];
                      return false;
                    }
                    if (
                      !(
                        data4 === "initial" ||
                        data4 === "change" ||
                        data4 === "reconnect" ||
                        data4 === "overflow" ||
                        data4 === "workspace"
                      )
                    ) {
                      validate22.errors = [
                        {
                          instancePath: instancePath + "/reason",
                          schemaPath: "#/properties/reason/enum",
                          keyword: "enum",
                          params: {
                            allowedValues: schema54.properties.reason.enum,
                          },
                          message: "must be equal to one of the allowed values",
                        },
                      ];
                      return false;
                    }
                    var valid0 = _errs10 === errors;
                  } else {
                    var valid0 = true;
                  }
                  if (valid0) {
                    if (data.resources !== void 0) {
                      let data5 = data.resources;
                      const _errs12 = errors;
                      if (errors === _errs12) {
                        if (Array.isArray(data5)) {
                          var valid1 = true;
                          const len0 = data5.length;
                          for (let i0 = 0; i0 < len0; i0++) {
                            const _errs14 = errors;
                            if (
                              !validate21(data5[i0], {
                                instancePath: instancePath + "/resources/" + i0,
                                parentData: data5,
                                parentDataProperty: i0,
                                rootData,
                                dynamicAnchors,
                              })
                            ) {
                              vErrors =
                                vErrors === null
                                  ? validate21.errors
                                  : vErrors.concat(validate21.errors);
                              errors = vErrors.length;
                            }
                            var valid1 = _errs14 === errors;
                            if (!valid1) {
                              break;
                            }
                          }
                        } else {
                          validate22.errors = [
                            {
                              instancePath: instancePath + "/resources",
                              schemaPath: "#/properties/resources/type",
                              keyword: "type",
                              params: { type: "array" },
                              message: "must be array",
                            },
                          ];
                          return false;
                        }
                      }
                      var valid0 = _errs12 === errors;
                    } else {
                      var valid0 = true;
                    }
                  }
                }
              }
            }
          }
        }
      }
    } else {
      validate22.errors = [
        {
          instancePath,
          schemaPath: "#/type",
          keyword: "type",
          params: { type: "object" },
          message: "must be object",
        },
      ];
      return false;
    }
  }
  validate22.errors = vErrors;
  return errors === 0;
}
validate22.evaluated = {
  props: true,
  dynamicProps: false,
  dynamicItems: false,
};
var isResourceHeartbeatEvent = validate24;
function validate24(
  data,
  {
    instancePath = "",
    parentData,
    parentDataProperty,
    rootData = data,
    dynamicAnchors = {},
  } = {},
) {
  let vErrors = null;
  let errors = 0;
  const evaluated0 = validate24.evaluated;
  if (evaluated0.dynamicProps) {
    evaluated0.props = void 0;
  }
  if (evaluated0.dynamicItems) {
    evaluated0.items = void 0;
  }
  if (errors === 0) {
    if (data && typeof data == "object" && !Array.isArray(data)) {
      let missing0;
      if (
        (data.protocol === void 0 && (missing0 = "protocol")) ||
        (data.workspaceId === void 0 && (missing0 = "workspaceId")) ||
        (data.epoch === void 0 && (missing0 = "epoch")) ||
        (data.revision === void 0 && (missing0 = "revision"))
      ) {
        validate24.errors = [
          {
            instancePath,
            schemaPath: "#/required",
            keyword: "required",
            params: { missingProperty: missing0 },
            message: "must have required property '" + missing0 + "'",
          },
        ];
        return false;
      } else {
        const _errs1 = errors;
        for (const key0 in data) {
          if (
            !(
              key0 === "protocol" ||
              key0 === "workspaceId" ||
              key0 === "epoch" ||
              key0 === "revision"
            )
          ) {
            validate24.errors = [
              {
                instancePath,
                schemaPath: "#/additionalProperties",
                keyword: "additionalProperties",
                params: { additionalProperty: key0 },
                message: "must NOT have additional properties",
              },
            ];
            return false;
            break;
          }
        }
        if (_errs1 === errors) {
          if (data.protocol !== void 0) {
            let data0 = data.protocol;
            const _errs2 = errors;
            if (
              !(
                typeof data0 == "number" &&
                !(data0 % 1) &&
                !isNaN(data0) &&
                isFinite(data0)
              )
            ) {
              validate24.errors = [
                {
                  instancePath: instancePath + "/protocol",
                  schemaPath: "#/properties/protocol/type",
                  keyword: "type",
                  params: { type: "integer" },
                  message: "must be integer",
                },
              ];
              return false;
            }
            if (3 !== data0) {
              validate24.errors = [
                {
                  instancePath: instancePath + "/protocol",
                  schemaPath: "#/properties/protocol/const",
                  keyword: "const",
                  params: { allowedValue: 3 },
                  message: "must be equal to constant",
                },
              ];
              return false;
            }
            var valid0 = _errs2 === errors;
          } else {
            var valid0 = true;
          }
          if (valid0) {
            if (data.workspaceId !== void 0) {
              let data1 = data.workspaceId;
              const _errs4 = errors;
              if (errors === _errs4) {
                if (typeof data1 === "string") {
                  if (func1(data1) < 1) {
                    validate24.errors = [
                      {
                        instancePath: instancePath + "/workspaceId",
                        schemaPath: "#/properties/workspaceId/minLength",
                        keyword: "minLength",
                        params: { limit: 1 },
                        message: "must NOT have fewer than 1 characters",
                      },
                    ];
                    return false;
                  }
                } else {
                  validate24.errors = [
                    {
                      instancePath: instancePath + "/workspaceId",
                      schemaPath: "#/properties/workspaceId/type",
                      keyword: "type",
                      params: { type: "string" },
                      message: "must be string",
                    },
                  ];
                  return false;
                }
              }
              var valid0 = _errs4 === errors;
            } else {
              var valid0 = true;
            }
            if (valid0) {
              if (data.epoch !== void 0) {
                let data2 = data.epoch;
                const _errs6 = errors;
                if (errors === _errs6) {
                  if (typeof data2 === "string") {
                    if (func1(data2) < 1) {
                      validate24.errors = [
                        {
                          instancePath: instancePath + "/epoch",
                          schemaPath: "#/properties/epoch/minLength",
                          keyword: "minLength",
                          params: { limit: 1 },
                          message: "must NOT have fewer than 1 characters",
                        },
                      ];
                      return false;
                    }
                  } else {
                    validate24.errors = [
                      {
                        instancePath: instancePath + "/epoch",
                        schemaPath: "#/properties/epoch/type",
                        keyword: "type",
                        params: { type: "string" },
                        message: "must be string",
                      },
                    ];
                    return false;
                  }
                }
                var valid0 = _errs6 === errors;
              } else {
                var valid0 = true;
              }
              if (valid0) {
                if (data.revision !== void 0) {
                  let data3 = data.revision;
                  const _errs8 = errors;
                  if (
                    !(
                      typeof data3 == "number" &&
                      !(data3 % 1) &&
                      !isNaN(data3) &&
                      isFinite(data3)
                    )
                  ) {
                    validate24.errors = [
                      {
                        instancePath: instancePath + "/revision",
                        schemaPath: "#/properties/revision/type",
                        keyword: "type",
                        params: { type: "integer" },
                        message: "must be integer",
                      },
                    ];
                    return false;
                  }
                  if (errors === _errs8) {
                    if (typeof data3 == "number" && isFinite(data3)) {
                      if (data3 > 9007199254740991 || isNaN(data3)) {
                        validate24.errors = [
                          {
                            instancePath: instancePath + "/revision",
                            schemaPath: "#/properties/revision/maximum",
                            keyword: "maximum",
                            params: {
                              comparison: "<=",
                              limit: 9007199254740991,
                            },
                            message: "must be <= 9007199254740991",
                          },
                        ];
                        return false;
                      } else {
                        if (data3 < 0 || isNaN(data3)) {
                          validate24.errors = [
                            {
                              instancePath: instancePath + "/revision",
                              schemaPath: "#/properties/revision/minimum",
                              keyword: "minimum",
                              params: { comparison: ">=", limit: 0 },
                              message: "must be >= 0",
                            },
                          ];
                          return false;
                        }
                      }
                    }
                  }
                  var valid0 = _errs8 === errors;
                } else {
                  var valid0 = true;
                }
              }
            }
          }
        }
      }
    } else {
      validate24.errors = [
        {
          instancePath,
          schemaPath: "#/type",
          keyword: "type",
          params: { type: "object" },
          message: "must be object",
        },
      ];
      return false;
    }
  }
  validate24.errors = vErrors;
  return errors === 0;
}
validate24.evaluated = {
  props: true,
  dynamicProps: false,
  dynamicItems: false,
};
var isResourceTokenRatesEvent = validate25;
function validate25(
  data,
  {
    instancePath = "",
    parentData,
    parentDataProperty,
    rootData = data,
    dynamicAnchors = {},
  } = {},
) {
  let vErrors = null;
  let errors = 0;
  const evaluated0 = validate25.evaluated;
  if (evaluated0.dynamicProps) {
    evaluated0.props = void 0;
  }
  if (evaluated0.dynamicItems) {
    evaluated0.items = void 0;
  }
  if (errors === 0) {
    if (data && typeof data == "object" && !Array.isArray(data)) {
      let missing0;
      if (
        (data.protocol === void 0 && (missing0 = "protocol")) ||
        (data.workspaceId === void 0 && (missing0 = "workspaceId")) ||
        (data.epoch === void 0 && (missing0 = "epoch")) ||
        (data.revision === void 0 && (missing0 = "revision")) ||
        (data.rates === void 0 && (missing0 = "rates")) ||
        (data.teams === void 0 && (missing0 = "teams"))
      ) {
        validate25.errors = [
          {
            instancePath,
            schemaPath: "#/required",
            keyword: "required",
            params: { missingProperty: missing0 },
            message: "must have required property '" + missing0 + "'",
          },
        ];
        return false;
      } else {
        const _errs1 = errors;
        for (const key0 in data) {
          if (
            !(
              key0 === "protocol" ||
              key0 === "workspaceId" ||
              key0 === "epoch" ||
              key0 === "revision" ||
              key0 === "rates" ||
              key0 === "teams"
            )
          ) {
            validate25.errors = [
              {
                instancePath,
                schemaPath: "#/additionalProperties",
                keyword: "additionalProperties",
                params: { additionalProperty: key0 },
                message: "must NOT have additional properties",
              },
            ];
            return false;
            break;
          }
        }
        if (_errs1 === errors) {
          if (data.protocol !== void 0) {
            let data0 = data.protocol;
            const _errs2 = errors;
            if (
              !(
                typeof data0 == "number" &&
                !(data0 % 1) &&
                !isNaN(data0) &&
                isFinite(data0)
              )
            ) {
              validate25.errors = [
                {
                  instancePath: instancePath + "/protocol",
                  schemaPath: "#/properties/protocol/type",
                  keyword: "type",
                  params: { type: "integer" },
                  message: "must be integer",
                },
              ];
              return false;
            }
            if (3 !== data0) {
              validate25.errors = [
                {
                  instancePath: instancePath + "/protocol",
                  schemaPath: "#/properties/protocol/const",
                  keyword: "const",
                  params: { allowedValue: 3 },
                  message: "must be equal to constant",
                },
              ];
              return false;
            }
            var valid0 = _errs2 === errors;
          } else {
            var valid0 = true;
          }
          if (valid0) {
            if (data.workspaceId !== void 0) {
              let data1 = data.workspaceId;
              const _errs4 = errors;
              if (errors === _errs4) {
                if (typeof data1 === "string") {
                  if (func1(data1) < 1) {
                    validate25.errors = [
                      {
                        instancePath: instancePath + "/workspaceId",
                        schemaPath: "#/properties/workspaceId/minLength",
                        keyword: "minLength",
                        params: { limit: 1 },
                        message: "must NOT have fewer than 1 characters",
                      },
                    ];
                    return false;
                  }
                } else {
                  validate25.errors = [
                    {
                      instancePath: instancePath + "/workspaceId",
                      schemaPath: "#/properties/workspaceId/type",
                      keyword: "type",
                      params: { type: "string" },
                      message: "must be string",
                    },
                  ];
                  return false;
                }
              }
              var valid0 = _errs4 === errors;
            } else {
              var valid0 = true;
            }
            if (valid0) {
              if (data.epoch !== void 0) {
                let data2 = data.epoch;
                const _errs6 = errors;
                if (errors === _errs6) {
                  if (typeof data2 === "string") {
                    if (func1(data2) < 1) {
                      validate25.errors = [
                        {
                          instancePath: instancePath + "/epoch",
                          schemaPath: "#/properties/epoch/minLength",
                          keyword: "minLength",
                          params: { limit: 1 },
                          message: "must NOT have fewer than 1 characters",
                        },
                      ];
                      return false;
                    }
                  } else {
                    validate25.errors = [
                      {
                        instancePath: instancePath + "/epoch",
                        schemaPath: "#/properties/epoch/type",
                        keyword: "type",
                        params: { type: "string" },
                        message: "must be string",
                      },
                    ];
                    return false;
                  }
                }
                var valid0 = _errs6 === errors;
              } else {
                var valid0 = true;
              }
              if (valid0) {
                if (data.revision !== void 0) {
                  let data3 = data.revision;
                  const _errs8 = errors;
                  if (
                    !(
                      typeof data3 == "number" &&
                      !(data3 % 1) &&
                      !isNaN(data3) &&
                      isFinite(data3)
                    )
                  ) {
                    validate25.errors = [
                      {
                        instancePath: instancePath + "/revision",
                        schemaPath: "#/properties/revision/type",
                        keyword: "type",
                        params: { type: "integer" },
                        message: "must be integer",
                      },
                    ];
                    return false;
                  }
                  if (errors === _errs8) {
                    if (typeof data3 == "number" && isFinite(data3)) {
                      if (data3 > 9007199254740991 || isNaN(data3)) {
                        validate25.errors = [
                          {
                            instancePath: instancePath + "/revision",
                            schemaPath: "#/properties/revision/maximum",
                            keyword: "maximum",
                            params: {
                              comparison: "<=",
                              limit: 9007199254740991,
                            },
                            message: "must be <= 9007199254740991",
                          },
                        ];
                        return false;
                      } else {
                        if (data3 < 0 || isNaN(data3)) {
                          validate25.errors = [
                            {
                              instancePath: instancePath + "/revision",
                              schemaPath: "#/properties/revision/minimum",
                              keyword: "minimum",
                              params: { comparison: ">=", limit: 0 },
                              message: "must be >= 0",
                            },
                          ];
                          return false;
                        }
                      }
                    }
                  }
                  var valid0 = _errs8 === errors;
                } else {
                  var valid0 = true;
                }
                if (valid0) {
                  if (data.rates !== void 0) {
                    let data4 = data.rates;
                    const _errs10 = errors;
                    if (errors === _errs10) {
                      if (
                        data4 &&
                        typeof data4 == "object" &&
                        !Array.isArray(data4)
                      ) {
                        for (const key1 in data4) {
                          const _errs12 = errors;
                          if (typeof key1 === "string") {
                            if (func1(key1) < 1) {
                              const err0 = {
                                instancePath: instancePath + "/rates",
                                schemaPath:
                                  "#/properties/rates/propertyNames/minLength",
                                keyword: "minLength",
                                params: { limit: 1 },
                                message:
                                  "must NOT have fewer than 1 characters",
                                propertyName: key1,
                              };
                              if (vErrors === null) {
                                vErrors = [err0];
                              } else {
                                vErrors.push(err0);
                              }
                              errors++;
                            }
                          }
                          var valid1 = _errs12 === errors;
                          if (!valid1) {
                            const err1 = {
                              instancePath: instancePath + "/rates",
                              schemaPath: "#/properties/rates/propertyNames",
                              keyword: "propertyNames",
                              params: { propertyName: key1 },
                              message: "property name must be valid",
                            };
                            if (vErrors === null) {
                              vErrors = [err1];
                            } else {
                              vErrors.push(err1);
                            }
                            errors++;
                            validate25.errors = vErrors;
                            return false;
                            break;
                          }
                        }
                        if (valid1) {
                          for (const key2 in data4) {
                            let data5 = data4[key2];
                            const _errs14 = errors;
                            const _errs15 = errors;
                            if (errors === _errs15) {
                              if (
                                data5 &&
                                typeof data5 == "object" &&
                                !Array.isArray(data5)
                              ) {
                                let missing1;
                                if (
                                  (data5.turnId === void 0 &&
                                    (missing1 = "turnId")) ||
                                  (data5.active === void 0 &&
                                    (missing1 = "active")) ||
                                  (data5.estimated === void 0 &&
                                    (missing1 = "estimated")) ||
                                  (data5.rate === void 0 &&
                                    (missing1 = "rate")) ||
                                  (data5.outputTokens === void 0 &&
                                    (missing1 = "outputTokens"))
                                ) {
                                  validate25.errors = [
                                    {
                                      instancePath:
                                        instancePath +
                                        "/rates/" +
                                        key2
                                          .replace(/~/g, "~0")
                                          .replace(/\//g, "~1"),
                                      schemaPath:
                                        "#/$defs/TokenRateValue/required",
                                      keyword: "required",
                                      params: { missingProperty: missing1 },
                                      message:
                                        "must have required property '" +
                                        missing1 +
                                        "'",
                                    },
                                  ];
                                  return false;
                                } else {
                                  const _errs17 = errors;
                                  for (const key3 in data5) {
                                    if (
                                      !(
                                        key3 === "turnId" ||
                                        key3 === "active" ||
                                        key3 === "estimated" ||
                                        key3 === "rate" ||
                                        key3 === "outputTokens"
                                      )
                                    ) {
                                      validate25.errors = [
                                        {
                                          instancePath:
                                            instancePath +
                                            "/rates/" +
                                            key2
                                              .replace(/~/g, "~0")
                                              .replace(/\//g, "~1"),
                                          schemaPath:
                                            "#/$defs/TokenRateValue/additionalProperties",
                                          keyword: "additionalProperties",
                                          params: { additionalProperty: key3 },
                                          message:
                                            "must NOT have additional properties",
                                        },
                                      ];
                                      return false;
                                      break;
                                    }
                                  }
                                  if (_errs17 === errors) {
                                    if (data5.turnId !== void 0) {
                                      let data6 = data5.turnId;
                                      const _errs18 = errors;
                                      if (errors === _errs18) {
                                        if (typeof data6 === "string") {
                                          if (func1(data6) < 1) {
                                            validate25.errors = [
                                              {
                                                instancePath:
                                                  instancePath +
                                                  "/rates/" +
                                                  key2
                                                    .replace(/~/g, "~0")
                                                    .replace(/\//g, "~1") +
                                                  "/turnId",
                                                schemaPath:
                                                  "#/$defs/TokenRateValue/properties/turnId/minLength",
                                                keyword: "minLength",
                                                params: { limit: 1 },
                                                message:
                                                  "must NOT have fewer than 1 characters",
                                              },
                                            ];
                                            return false;
                                          }
                                        } else {
                                          validate25.errors = [
                                            {
                                              instancePath:
                                                instancePath +
                                                "/rates/" +
                                                key2
                                                  .replace(/~/g, "~0")
                                                  .replace(/\//g, "~1") +
                                                "/turnId",
                                              schemaPath:
                                                "#/$defs/TokenRateValue/properties/turnId/type",
                                              keyword: "type",
                                              params: { type: "string" },
                                              message: "must be string",
                                            },
                                          ];
                                          return false;
                                        }
                                      }
                                      var valid4 = _errs18 === errors;
                                    } else {
                                      var valid4 = true;
                                    }
                                    if (valid4) {
                                      if (data5.active !== void 0) {
                                        const _errs20 = errors;
                                        if (typeof data5.active !== "boolean") {
                                          validate25.errors = [
                                            {
                                              instancePath:
                                                instancePath +
                                                "/rates/" +
                                                key2
                                                  .replace(/~/g, "~0")
                                                  .replace(/\//g, "~1") +
                                                "/active",
                                              schemaPath:
                                                "#/$defs/TokenRateValue/properties/active/type",
                                              keyword: "type",
                                              params: { type: "boolean" },
                                              message: "must be boolean",
                                            },
                                          ];
                                          return false;
                                        }
                                        var valid4 = _errs20 === errors;
                                      } else {
                                        var valid4 = true;
                                      }
                                      if (valid4) {
                                        if (data5.estimated !== void 0) {
                                          const _errs22 = errors;
                                          if (
                                            typeof data5.estimated !== "boolean"
                                          ) {
                                            validate25.errors = [
                                              {
                                                instancePath:
                                                  instancePath +
                                                  "/rates/" +
                                                  key2
                                                    .replace(/~/g, "~0")
                                                    .replace(/\//g, "~1") +
                                                  "/estimated",
                                                schemaPath:
                                                  "#/$defs/TokenRateValue/properties/estimated/type",
                                                keyword: "type",
                                                params: { type: "boolean" },
                                                message: "must be boolean",
                                              },
                                            ];
                                            return false;
                                          }
                                          var valid4 = _errs22 === errors;
                                        } else {
                                          var valid4 = true;
                                        }
                                        if (valid4) {
                                          if (data5.rate !== void 0) {
                                            let data9 = data5.rate;
                                            const _errs24 = errors;
                                            const _errs25 = errors;
                                            let valid5 = false;
                                            const _errs26 = errors;
                                            if (
                                              !(
                                                typeof data9 == "number" &&
                                                !(data9 % 1) &&
                                                !isNaN(data9) &&
                                                isFinite(data9)
                                              )
                                            ) {
                                              const err2 = {
                                                instancePath:
                                                  instancePath +
                                                  "/rates/" +
                                                  key2
                                                    .replace(/~/g, "~0")
                                                    .replace(/\//g, "~1") +
                                                  "/rate",
                                                schemaPath:
                                                  "#/$defs/TokenRateValue/properties/rate/anyOf/0/type",
                                                keyword: "type",
                                                params: { type: "integer" },
                                                message: "must be integer",
                                              };
                                              if (vErrors === null) {
                                                vErrors = [err2];
                                              } else {
                                                vErrors.push(err2);
                                              }
                                              errors++;
                                            }
                                            if (errors === _errs26) {
                                              if (
                                                typeof data9 == "number" &&
                                                isFinite(data9)
                                              ) {
                                                if (data9 < 0 || isNaN(data9)) {
                                                  const err3 = {
                                                    instancePath:
                                                      instancePath +
                                                      "/rates/" +
                                                      key2
                                                        .replace(/~/g, "~0")
                                                        .replace(/\//g, "~1") +
                                                      "/rate",
                                                    schemaPath:
                                                      "#/$defs/TokenRateValue/properties/rate/anyOf/0/minimum",
                                                    keyword: "minimum",
                                                    params: {
                                                      comparison: ">=",
                                                      limit: 0,
                                                    },
                                                    message: "must be >= 0",
                                                  };
                                                  if (vErrors === null) {
                                                    vErrors = [err3];
                                                  } else {
                                                    vErrors.push(err3);
                                                  }
                                                  errors++;
                                                }
                                              }
                                            }
                                            var _valid0 = _errs26 === errors;
                                            valid5 = valid5 || _valid0;
                                            const _errs28 = errors;
                                            if (errors === _errs28) {
                                              if (
                                                typeof data9 == "number" &&
                                                isFinite(data9)
                                              ) {
                                                if (data9 < 0 || isNaN(data9)) {
                                                  const err4 = {
                                                    instancePath:
                                                      instancePath +
                                                      "/rates/" +
                                                      key2
                                                        .replace(/~/g, "~0")
                                                        .replace(/\//g, "~1") +
                                                      "/rate",
                                                    schemaPath:
                                                      "#/$defs/TokenRateValue/properties/rate/anyOf/1/minimum",
                                                    keyword: "minimum",
                                                    params: {
                                                      comparison: ">=",
                                                      limit: 0,
                                                    },
                                                    message: "must be >= 0",
                                                  };
                                                  if (vErrors === null) {
                                                    vErrors = [err4];
                                                  } else {
                                                    vErrors.push(err4);
                                                  }
                                                  errors++;
                                                }
                                              } else {
                                                const err5 = {
                                                  instancePath:
                                                    instancePath +
                                                    "/rates/" +
                                                    key2
                                                      .replace(/~/g, "~0")
                                                      .replace(/\//g, "~1") +
                                                    "/rate",
                                                  schemaPath:
                                                    "#/$defs/TokenRateValue/properties/rate/anyOf/1/type",
                                                  keyword: "type",
                                                  params: { type: "number" },
                                                  message: "must be number",
                                                };
                                                if (vErrors === null) {
                                                  vErrors = [err5];
                                                } else {
                                                  vErrors.push(err5);
                                                }
                                                errors++;
                                              }
                                            }
                                            var _valid0 = _errs28 === errors;
                                            valid5 = valid5 || _valid0;
                                            if (!valid5) {
                                              const err6 = {
                                                instancePath:
                                                  instancePath +
                                                  "/rates/" +
                                                  key2
                                                    .replace(/~/g, "~0")
                                                    .replace(/\//g, "~1") +
                                                  "/rate",
                                                schemaPath:
                                                  "#/$defs/TokenRateValue/properties/rate/anyOf",
                                                keyword: "anyOf",
                                                params: {},
                                                message:
                                                  "must match a schema in anyOf",
                                              };
                                              if (vErrors === null) {
                                                vErrors = [err6];
                                              } else {
                                                vErrors.push(err6);
                                              }
                                              errors++;
                                              validate25.errors = vErrors;
                                              return false;
                                            } else {
                                              errors = _errs25;
                                              if (vErrors !== null) {
                                                if (_errs25) {
                                                  vErrors.length = _errs25;
                                                } else {
                                                  vErrors = null;
                                                }
                                              }
                                            }
                                            var valid4 = _errs24 === errors;
                                          } else {
                                            var valid4 = true;
                                          }
                                          if (valid4) {
                                            if (data5.outputTokens !== void 0) {
                                              let data10 = data5.outputTokens;
                                              const _errs30 = errors;
                                              const _errs31 = errors;
                                              let valid6 = false;
                                              const _errs32 = errors;
                                              if (
                                                !(
                                                  typeof data10 == "number" &&
                                                  !(data10 % 1) &&
                                                  !isNaN(data10) &&
                                                  isFinite(data10)
                                                )
                                              ) {
                                                const err7 = {
                                                  instancePath:
                                                    instancePath +
                                                    "/rates/" +
                                                    key2
                                                      .replace(/~/g, "~0")
                                                      .replace(/\//g, "~1") +
                                                    "/outputTokens",
                                                  schemaPath:
                                                    "#/$defs/TokenRateValue/properties/outputTokens/anyOf/0/type",
                                                  keyword: "type",
                                                  params: { type: "integer" },
                                                  message: "must be integer",
                                                };
                                                if (vErrors === null) {
                                                  vErrors = [err7];
                                                } else {
                                                  vErrors.push(err7);
                                                }
                                                errors++;
                                              }
                                              if (errors === _errs32) {
                                                if (
                                                  typeof data10 == "number" &&
                                                  isFinite(data10)
                                                ) {
                                                  if (
                                                    data10 < 0 ||
                                                    isNaN(data10)
                                                  ) {
                                                    const err8 = {
                                                      instancePath:
                                                        instancePath +
                                                        "/rates/" +
                                                        key2
                                                          .replace(/~/g, "~0")
                                                          .replace(
                                                            /\//g,
                                                            "~1",
                                                          ) +
                                                        "/outputTokens",
                                                      schemaPath:
                                                        "#/$defs/TokenRateValue/properties/outputTokens/anyOf/0/minimum",
                                                      keyword: "minimum",
                                                      params: {
                                                        comparison: ">=",
                                                        limit: 0,
                                                      },
                                                      message: "must be >= 0",
                                                    };
                                                    if (vErrors === null) {
                                                      vErrors = [err8];
                                                    } else {
                                                      vErrors.push(err8);
                                                    }
                                                    errors++;
                                                  }
                                                }
                                              }
                                              var _valid1 = _errs32 === errors;
                                              valid6 = valid6 || _valid1;
                                              const _errs34 = errors;
                                              if (errors === _errs34) {
                                                if (
                                                  typeof data10 == "number" &&
                                                  isFinite(data10)
                                                ) {
                                                  if (
                                                    data10 < 0 ||
                                                    isNaN(data10)
                                                  ) {
                                                    const err9 = {
                                                      instancePath:
                                                        instancePath +
                                                        "/rates/" +
                                                        key2
                                                          .replace(/~/g, "~0")
                                                          .replace(
                                                            /\//g,
                                                            "~1",
                                                          ) +
                                                        "/outputTokens",
                                                      schemaPath:
                                                        "#/$defs/TokenRateValue/properties/outputTokens/anyOf/1/minimum",
                                                      keyword: "minimum",
                                                      params: {
                                                        comparison: ">=",
                                                        limit: 0,
                                                      },
                                                      message: "must be >= 0",
                                                    };
                                                    if (vErrors === null) {
                                                      vErrors = [err9];
                                                    } else {
                                                      vErrors.push(err9);
                                                    }
                                                    errors++;
                                                  }
                                                } else {
                                                  const err10 = {
                                                    instancePath:
                                                      instancePath +
                                                      "/rates/" +
                                                      key2
                                                        .replace(/~/g, "~0")
                                                        .replace(/\//g, "~1") +
                                                      "/outputTokens",
                                                    schemaPath:
                                                      "#/$defs/TokenRateValue/properties/outputTokens/anyOf/1/type",
                                                    keyword: "type",
                                                    params: { type: "number" },
                                                    message: "must be number",
                                                  };
                                                  if (vErrors === null) {
                                                    vErrors = [err10];
                                                  } else {
                                                    vErrors.push(err10);
                                                  }
                                                  errors++;
                                                }
                                              }
                                              var _valid1 = _errs34 === errors;
                                              valid6 = valid6 || _valid1;
                                              if (!valid6) {
                                                const err11 = {
                                                  instancePath:
                                                    instancePath +
                                                    "/rates/" +
                                                    key2
                                                      .replace(/~/g, "~0")
                                                      .replace(/\//g, "~1") +
                                                    "/outputTokens",
                                                  schemaPath:
                                                    "#/$defs/TokenRateValue/properties/outputTokens/anyOf",
                                                  keyword: "anyOf",
                                                  params: {},
                                                  message:
                                                    "must match a schema in anyOf",
                                                };
                                                if (vErrors === null) {
                                                  vErrors = [err11];
                                                } else {
                                                  vErrors.push(err11);
                                                }
                                                errors++;
                                                validate25.errors = vErrors;
                                                return false;
                                              } else {
                                                errors = _errs31;
                                                if (vErrors !== null) {
                                                  if (_errs31) {
                                                    vErrors.length = _errs31;
                                                  } else {
                                                    vErrors = null;
                                                  }
                                                }
                                              }
                                              var valid4 = _errs30 === errors;
                                            } else {
                                              var valid4 = true;
                                            }
                                          }
                                        }
                                      }
                                    }
                                  }
                                }
                              } else {
                                validate25.errors = [
                                  {
                                    instancePath:
                                      instancePath +
                                      "/rates/" +
                                      key2
                                        .replace(/~/g, "~0")
                                        .replace(/\//g, "~1"),
                                    schemaPath: "#/$defs/TokenRateValue/type",
                                    keyword: "type",
                                    params: { type: "object" },
                                    message: "must be object",
                                  },
                                ];
                                return false;
                              }
                            }
                            var valid2 = _errs14 === errors;
                            if (!valid2) {
                              break;
                            }
                          }
                        }
                      } else {
                        validate25.errors = [
                          {
                            instancePath: instancePath + "/rates",
                            schemaPath: "#/properties/rates/type",
                            keyword: "type",
                            params: { type: "object" },
                            message: "must be object",
                          },
                        ];
                        return false;
                      }
                    }
                    var valid0 = _errs10 === errors;
                  } else {
                    var valid0 = true;
                  }
                  if (valid0) {
                    if (data.teams !== void 0) {
                      let data11 = data.teams;
                      const _errs36 = errors;
                      if (errors === _errs36) {
                        if (
                          data11 &&
                          typeof data11 == "object" &&
                          !Array.isArray(data11)
                        ) {
                          for (const key4 in data11) {
                            const _errs38 = errors;
                            if (typeof key4 === "string") {
                              if (func1(key4) < 1) {
                                const err12 = {
                                  instancePath: instancePath + "/teams",
                                  schemaPath:
                                    "#/properties/teams/propertyNames/minLength",
                                  keyword: "minLength",
                                  params: { limit: 1 },
                                  message:
                                    "must NOT have fewer than 1 characters",
                                  propertyName: key4,
                                };
                                if (vErrors === null) {
                                  vErrors = [err12];
                                } else {
                                  vErrors.push(err12);
                                }
                                errors++;
                              }
                            }
                            var valid7 = _errs38 === errors;
                            if (!valid7) {
                              const err13 = {
                                instancePath: instancePath + "/teams",
                                schemaPath: "#/properties/teams/propertyNames",
                                keyword: "propertyNames",
                                params: { propertyName: key4 },
                                message: "property name must be valid",
                              };
                              if (vErrors === null) {
                                vErrors = [err13];
                              } else {
                                vErrors.push(err13);
                              }
                              errors++;
                              validate25.errors = vErrors;
                              return false;
                              break;
                            }
                          }
                          if (valid7) {
                            for (const key5 in data11) {
                              let data12 = data11[key5];
                              const _errs40 = errors;
                              if (errors === _errs40) {
                                if (
                                  data12 &&
                                  typeof data12 == "object" &&
                                  !Array.isArray(data12)
                                ) {
                                  for (const key6 in data12) {
                                    const _errs42 = errors;
                                    if (typeof key6 === "string") {
                                      if (func1(key6) < 1) {
                                        const err14 = {
                                          instancePath:
                                            instancePath +
                                            "/teams/" +
                                            key5
                                              .replace(/~/g, "~0")
                                              .replace(/\//g, "~1"),
                                          schemaPath:
                                            "#/properties/teams/additionalProperties/propertyNames/minLength",
                                          keyword: "minLength",
                                          params: { limit: 1 },
                                          message:
                                            "must NOT have fewer than 1 characters",
                                          propertyName: key6,
                                        };
                                        if (vErrors === null) {
                                          vErrors = [err14];
                                        } else {
                                          vErrors.push(err14);
                                        }
                                        errors++;
                                      }
                                    }
                                    var valid9 = _errs42 === errors;
                                    if (!valid9) {
                                      const err15 = {
                                        instancePath:
                                          instancePath +
                                          "/teams/" +
                                          key5
                                            .replace(/~/g, "~0")
                                            .replace(/\//g, "~1"),
                                        schemaPath:
                                          "#/properties/teams/additionalProperties/propertyNames",
                                        keyword: "propertyNames",
                                        params: { propertyName: key6 },
                                        message: "property name must be valid",
                                      };
                                      if (vErrors === null) {
                                        vErrors = [err15];
                                      } else {
                                        vErrors.push(err15);
                                      }
                                      errors++;
                                      validate25.errors = vErrors;
                                      return false;
                                      break;
                                    }
                                  }
                                  if (valid9) {
                                    for (const key7 in data12) {
                                      let data13 = data12[key7];
                                      const _errs44 = errors;
                                      const _errs45 = errors;
                                      if (errors === _errs45) {
                                        if (
                                          data13 &&
                                          typeof data13 == "object" &&
                                          !Array.isArray(data13)
                                        ) {
                                          let missing2;
                                          if (
                                            (data13.turnId === void 0 &&
                                              (missing2 = "turnId")) ||
                                            (data13.active === void 0 &&
                                              (missing2 = "active")) ||
                                            (data13.estimated === void 0 &&
                                              (missing2 = "estimated")) ||
                                            (data13.rate === void 0 &&
                                              (missing2 = "rate")) ||
                                            (data13.outputTokens === void 0 &&
                                              (missing2 = "outputTokens"))
                                          ) {
                                            validate25.errors = [
                                              {
                                                instancePath:
                                                  instancePath +
                                                  "/teams/" +
                                                  key5
                                                    .replace(/~/g, "~0")
                                                    .replace(/\//g, "~1") +
                                                  "/" +
                                                  key7
                                                    .replace(/~/g, "~0")
                                                    .replace(/\//g, "~1"),
                                                schemaPath:
                                                  "#/$defs/TokenRateValue/required",
                                                keyword: "required",
                                                params: {
                                                  missingProperty: missing2,
                                                },
                                                message:
                                                  "must have required property '" +
                                                  missing2 +
                                                  "'",
                                              },
                                            ];
                                            return false;
                                          } else {
                                            const _errs47 = errors;
                                            for (const key8 in data13) {
                                              if (
                                                !(
                                                  key8 === "turnId" ||
                                                  key8 === "active" ||
                                                  key8 === "estimated" ||
                                                  key8 === "rate" ||
                                                  key8 === "outputTokens"
                                                )
                                              ) {
                                                validate25.errors = [
                                                  {
                                                    instancePath:
                                                      instancePath +
                                                      "/teams/" +
                                                      key5
                                                        .replace(/~/g, "~0")
                                                        .replace(/\//g, "~1") +
                                                      "/" +
                                                      key7
                                                        .replace(/~/g, "~0")
                                                        .replace(/\//g, "~1"),
                                                    schemaPath:
                                                      "#/$defs/TokenRateValue/additionalProperties",
                                                    keyword:
                                                      "additionalProperties",
                                                    params: {
                                                      additionalProperty: key8,
                                                    },
                                                    message:
                                                      "must NOT have additional properties",
                                                  },
                                                ];
                                                return false;
                                                break;
                                              }
                                            }
                                            if (_errs47 === errors) {
                                              if (data13.turnId !== void 0) {
                                                let data14 = data13.turnId;
                                                const _errs48 = errors;
                                                if (errors === _errs48) {
                                                  if (
                                                    typeof data14 === "string"
                                                  ) {
                                                    if (func1(data14) < 1) {
                                                      validate25.errors = [
                                                        {
                                                          instancePath:
                                                            instancePath +
                                                            "/teams/" +
                                                            key5
                                                              .replace(
                                                                /~/g,
                                                                "~0",
                                                              )
                                                              .replace(
                                                                /\//g,
                                                                "~1",
                                                              ) +
                                                            "/" +
                                                            key7
                                                              .replace(
                                                                /~/g,
                                                                "~0",
                                                              )
                                                              .replace(
                                                                /\//g,
                                                                "~1",
                                                              ) +
                                                            "/turnId",
                                                          schemaPath:
                                                            "#/$defs/TokenRateValue/properties/turnId/minLength",
                                                          keyword: "minLength",
                                                          params: { limit: 1 },
                                                          message:
                                                            "must NOT have fewer than 1 characters",
                                                        },
                                                      ];
                                                      return false;
                                                    }
                                                  } else {
                                                    validate25.errors = [
                                                      {
                                                        instancePath:
                                                          instancePath +
                                                          "/teams/" +
                                                          key5
                                                            .replace(/~/g, "~0")
                                                            .replace(
                                                              /\//g,
                                                              "~1",
                                                            ) +
                                                          "/" +
                                                          key7
                                                            .replace(/~/g, "~0")
                                                            .replace(
                                                              /\//g,
                                                              "~1",
                                                            ) +
                                                          "/turnId",
                                                        schemaPath:
                                                          "#/$defs/TokenRateValue/properties/turnId/type",
                                                        keyword: "type",
                                                        params: {
                                                          type: "string",
                                                        },
                                                        message:
                                                          "must be string",
                                                      },
                                                    ];
                                                    return false;
                                                  }
                                                }
                                                var valid12 =
                                                  _errs48 === errors;
                                              } else {
                                                var valid12 = true;
                                              }
                                              if (valid12) {
                                                if (data13.active !== void 0) {
                                                  const _errs50 = errors;
                                                  if (
                                                    typeof data13.active !==
                                                    "boolean"
                                                  ) {
                                                    validate25.errors = [
                                                      {
                                                        instancePath:
                                                          instancePath +
                                                          "/teams/" +
                                                          key5
                                                            .replace(/~/g, "~0")
                                                            .replace(
                                                              /\//g,
                                                              "~1",
                                                            ) +
                                                          "/" +
                                                          key7
                                                            .replace(/~/g, "~0")
                                                            .replace(
                                                              /\//g,
                                                              "~1",
                                                            ) +
                                                          "/active",
                                                        schemaPath:
                                                          "#/$defs/TokenRateValue/properties/active/type",
                                                        keyword: "type",
                                                        params: {
                                                          type: "boolean",
                                                        },
                                                        message:
                                                          "must be boolean",
                                                      },
                                                    ];
                                                    return false;
                                                  }
                                                  var valid12 =
                                                    _errs50 === errors;
                                                } else {
                                                  var valid12 = true;
                                                }
                                                if (valid12) {
                                                  if (
                                                    data13.estimated !== void 0
                                                  ) {
                                                    const _errs52 = errors;
                                                    if (
                                                      typeof data13.estimated !==
                                                      "boolean"
                                                    ) {
                                                      validate25.errors = [
                                                        {
                                                          instancePath:
                                                            instancePath +
                                                            "/teams/" +
                                                            key5
                                                              .replace(
                                                                /~/g,
                                                                "~0",
                                                              )
                                                              .replace(
                                                                /\//g,
                                                                "~1",
                                                              ) +
                                                            "/" +
                                                            key7
                                                              .replace(
                                                                /~/g,
                                                                "~0",
                                                              )
                                                              .replace(
                                                                /\//g,
                                                                "~1",
                                                              ) +
                                                            "/estimated",
                                                          schemaPath:
                                                            "#/$defs/TokenRateValue/properties/estimated/type",
                                                          keyword: "type",
                                                          params: {
                                                            type: "boolean",
                                                          },
                                                          message:
                                                            "must be boolean",
                                                        },
                                                      ];
                                                      return false;
                                                    }
                                                    var valid12 =
                                                      _errs52 === errors;
                                                  } else {
                                                    var valid12 = true;
                                                  }
                                                  if (valid12) {
                                                    if (
                                                      data13.rate !== void 0
                                                    ) {
                                                      let data17 = data13.rate;
                                                      const _errs54 = errors;
                                                      const _errs55 = errors;
                                                      let valid13 = false;
                                                      const _errs56 = errors;
                                                      if (
                                                        !(
                                                          typeof data17 ==
                                                            "number" &&
                                                          !(data17 % 1) &&
                                                          !isNaN(data17) &&
                                                          isFinite(data17)
                                                        )
                                                      ) {
                                                        const err16 = {
                                                          instancePath:
                                                            instancePath +
                                                            "/teams/" +
                                                            key5
                                                              .replace(
                                                                /~/g,
                                                                "~0",
                                                              )
                                                              .replace(
                                                                /\//g,
                                                                "~1",
                                                              ) +
                                                            "/" +
                                                            key7
                                                              .replace(
                                                                /~/g,
                                                                "~0",
                                                              )
                                                              .replace(
                                                                /\//g,
                                                                "~1",
                                                              ) +
                                                            "/rate",
                                                          schemaPath:
                                                            "#/$defs/TokenRateValue/properties/rate/anyOf/0/type",
                                                          keyword: "type",
                                                          params: {
                                                            type: "integer",
                                                          },
                                                          message:
                                                            "must be integer",
                                                        };
                                                        if (vErrors === null) {
                                                          vErrors = [err16];
                                                        } else {
                                                          vErrors.push(err16);
                                                        }
                                                        errors++;
                                                      }
                                                      if (errors === _errs56) {
                                                        if (
                                                          typeof data17 ==
                                                            "number" &&
                                                          isFinite(data17)
                                                        ) {
                                                          if (
                                                            data17 < 0 ||
                                                            isNaN(data17)
                                                          ) {
                                                            const err17 = {
                                                              instancePath:
                                                                instancePath +
                                                                "/teams/" +
                                                                key5
                                                                  .replace(
                                                                    /~/g,
                                                                    "~0",
                                                                  )
                                                                  .replace(
                                                                    /\//g,
                                                                    "~1",
                                                                  ) +
                                                                "/" +
                                                                key7
                                                                  .replace(
                                                                    /~/g,
                                                                    "~0",
                                                                  )
                                                                  .replace(
                                                                    /\//g,
                                                                    "~1",
                                                                  ) +
                                                                "/rate",
                                                              schemaPath:
                                                                "#/$defs/TokenRateValue/properties/rate/anyOf/0/minimum",
                                                              keyword:
                                                                "minimum",
                                                              params: {
                                                                comparison:
                                                                  ">=",
                                                                limit: 0,
                                                              },
                                                              message:
                                                                "must be >= 0",
                                                            };
                                                            if (
                                                              vErrors === null
                                                            ) {
                                                              vErrors = [err17];
                                                            } else {
                                                              vErrors.push(
                                                                err17,
                                                              );
                                                            }
                                                            errors++;
                                                          }
                                                        }
                                                      }
                                                      var _valid2 =
                                                        _errs56 === errors;
                                                      valid13 =
                                                        valid13 || _valid2;
                                                      const _errs58 = errors;
                                                      if (errors === _errs58) {
                                                        if (
                                                          typeof data17 ==
                                                            "number" &&
                                                          isFinite(data17)
                                                        ) {
                                                          if (
                                                            data17 < 0 ||
                                                            isNaN(data17)
                                                          ) {
                                                            const err18 = {
                                                              instancePath:
                                                                instancePath +
                                                                "/teams/" +
                                                                key5
                                                                  .replace(
                                                                    /~/g,
                                                                    "~0",
                                                                  )
                                                                  .replace(
                                                                    /\//g,
                                                                    "~1",
                                                                  ) +
                                                                "/" +
                                                                key7
                                                                  .replace(
                                                                    /~/g,
                                                                    "~0",
                                                                  )
                                                                  .replace(
                                                                    /\//g,
                                                                    "~1",
                                                                  ) +
                                                                "/rate",
                                                              schemaPath:
                                                                "#/$defs/TokenRateValue/properties/rate/anyOf/1/minimum",
                                                              keyword:
                                                                "minimum",
                                                              params: {
                                                                comparison:
                                                                  ">=",
                                                                limit: 0,
                                                              },
                                                              message:
                                                                "must be >= 0",
                                                            };
                                                            if (
                                                              vErrors === null
                                                            ) {
                                                              vErrors = [err18];
                                                            } else {
                                                              vErrors.push(
                                                                err18,
                                                              );
                                                            }
                                                            errors++;
                                                          }
                                                        } else {
                                                          const err19 = {
                                                            instancePath:
                                                              instancePath +
                                                              "/teams/" +
                                                              key5
                                                                .replace(
                                                                  /~/g,
                                                                  "~0",
                                                                )
                                                                .replace(
                                                                  /\//g,
                                                                  "~1",
                                                                ) +
                                                              "/" +
                                                              key7
                                                                .replace(
                                                                  /~/g,
                                                                  "~0",
                                                                )
                                                                .replace(
                                                                  /\//g,
                                                                  "~1",
                                                                ) +
                                                              "/rate",
                                                            schemaPath:
                                                              "#/$defs/TokenRateValue/properties/rate/anyOf/1/type",
                                                            keyword: "type",
                                                            params: {
                                                              type: "number",
                                                            },
                                                            message:
                                                              "must be number",
                                                          };
                                                          if (
                                                            vErrors === null
                                                          ) {
                                                            vErrors = [err19];
                                                          } else {
                                                            vErrors.push(err19);
                                                          }
                                                          errors++;
                                                        }
                                                      }
                                                      var _valid2 =
                                                        _errs58 === errors;
                                                      valid13 =
                                                        valid13 || _valid2;
                                                      if (!valid13) {
                                                        const err20 = {
                                                          instancePath:
                                                            instancePath +
                                                            "/teams/" +
                                                            key5
                                                              .replace(
                                                                /~/g,
                                                                "~0",
                                                              )
                                                              .replace(
                                                                /\//g,
                                                                "~1",
                                                              ) +
                                                            "/" +
                                                            key7
                                                              .replace(
                                                                /~/g,
                                                                "~0",
                                                              )
                                                              .replace(
                                                                /\//g,
                                                                "~1",
                                                              ) +
                                                            "/rate",
                                                          schemaPath:
                                                            "#/$defs/TokenRateValue/properties/rate/anyOf",
                                                          keyword: "anyOf",
                                                          params: {},
                                                          message:
                                                            "must match a schema in anyOf",
                                                        };
                                                        if (vErrors === null) {
                                                          vErrors = [err20];
                                                        } else {
                                                          vErrors.push(err20);
                                                        }
                                                        errors++;
                                                        validate25.errors =
                                                          vErrors;
                                                        return false;
                                                      } else {
                                                        errors = _errs55;
                                                        if (vErrors !== null) {
                                                          if (_errs55) {
                                                            vErrors.length =
                                                              _errs55;
                                                          } else {
                                                            vErrors = null;
                                                          }
                                                        }
                                                      }
                                                      var valid12 =
                                                        _errs54 === errors;
                                                    } else {
                                                      var valid12 = true;
                                                    }
                                                    if (valid12) {
                                                      if (
                                                        data13.outputTokens !==
                                                        void 0
                                                      ) {
                                                        let data18 =
                                                          data13.outputTokens;
                                                        const _errs60 = errors;
                                                        const _errs61 = errors;
                                                        let valid14 = false;
                                                        const _errs62 = errors;
                                                        if (
                                                          !(
                                                            typeof data18 ==
                                                              "number" &&
                                                            !(data18 % 1) &&
                                                            !isNaN(data18) &&
                                                            isFinite(data18)
                                                          )
                                                        ) {
                                                          const err21 = {
                                                            instancePath:
                                                              instancePath +
                                                              "/teams/" +
                                                              key5
                                                                .replace(
                                                                  /~/g,
                                                                  "~0",
                                                                )
                                                                .replace(
                                                                  /\//g,
                                                                  "~1",
                                                                ) +
                                                              "/" +
                                                              key7
                                                                .replace(
                                                                  /~/g,
                                                                  "~0",
                                                                )
                                                                .replace(
                                                                  /\//g,
                                                                  "~1",
                                                                ) +
                                                              "/outputTokens",
                                                            schemaPath:
                                                              "#/$defs/TokenRateValue/properties/outputTokens/anyOf/0/type",
                                                            keyword: "type",
                                                            params: {
                                                              type: "integer",
                                                            },
                                                            message:
                                                              "must be integer",
                                                          };
                                                          if (
                                                            vErrors === null
                                                          ) {
                                                            vErrors = [err21];
                                                          } else {
                                                            vErrors.push(err21);
                                                          }
                                                          errors++;
                                                        }
                                                        if (
                                                          errors === _errs62
                                                        ) {
                                                          if (
                                                            typeof data18 ==
                                                              "number" &&
                                                            isFinite(data18)
                                                          ) {
                                                            if (
                                                              data18 < 0 ||
                                                              isNaN(data18)
                                                            ) {
                                                              const err22 = {
                                                                instancePath:
                                                                  instancePath +
                                                                  "/teams/" +
                                                                  key5
                                                                    .replace(
                                                                      /~/g,
                                                                      "~0",
                                                                    )
                                                                    .replace(
                                                                      /\//g,
                                                                      "~1",
                                                                    ) +
                                                                  "/" +
                                                                  key7
                                                                    .replace(
                                                                      /~/g,
                                                                      "~0",
                                                                    )
                                                                    .replace(
                                                                      /\//g,
                                                                      "~1",
                                                                    ) +
                                                                  "/outputTokens",
                                                                schemaPath:
                                                                  "#/$defs/TokenRateValue/properties/outputTokens/anyOf/0/minimum",
                                                                keyword:
                                                                  "minimum",
                                                                params: {
                                                                  comparison:
                                                                    ">=",
                                                                  limit: 0,
                                                                },
                                                                message:
                                                                  "must be >= 0",
                                                              };
                                                              if (
                                                                vErrors === null
                                                              ) {
                                                                vErrors = [
                                                                  err22,
                                                                ];
                                                              } else {
                                                                vErrors.push(
                                                                  err22,
                                                                );
                                                              }
                                                              errors++;
                                                            }
                                                          }
                                                        }
                                                        var _valid3 =
                                                          _errs62 === errors;
                                                        valid14 =
                                                          valid14 || _valid3;
                                                        const _errs64 = errors;
                                                        if (
                                                          errors === _errs64
                                                        ) {
                                                          if (
                                                            typeof data18 ==
                                                              "number" &&
                                                            isFinite(data18)
                                                          ) {
                                                            if (
                                                              data18 < 0 ||
                                                              isNaN(data18)
                                                            ) {
                                                              const err23 = {
                                                                instancePath:
                                                                  instancePath +
                                                                  "/teams/" +
                                                                  key5
                                                                    .replace(
                                                                      /~/g,
                                                                      "~0",
                                                                    )
                                                                    .replace(
                                                                      /\//g,
                                                                      "~1",
                                                                    ) +
                                                                  "/" +
                                                                  key7
                                                                    .replace(
                                                                      /~/g,
                                                                      "~0",
                                                                    )
                                                                    .replace(
                                                                      /\//g,
                                                                      "~1",
                                                                    ) +
                                                                  "/outputTokens",
                                                                schemaPath:
                                                                  "#/$defs/TokenRateValue/properties/outputTokens/anyOf/1/minimum",
                                                                keyword:
                                                                  "minimum",
                                                                params: {
                                                                  comparison:
                                                                    ">=",
                                                                  limit: 0,
                                                                },
                                                                message:
                                                                  "must be >= 0",
                                                              };
                                                              if (
                                                                vErrors === null
                                                              ) {
                                                                vErrors = [
                                                                  err23,
                                                                ];
                                                              } else {
                                                                vErrors.push(
                                                                  err23,
                                                                );
                                                              }
                                                              errors++;
                                                            }
                                                          } else {
                                                            const err24 = {
                                                              instancePath:
                                                                instancePath +
                                                                "/teams/" +
                                                                key5
                                                                  .replace(
                                                                    /~/g,
                                                                    "~0",
                                                                  )
                                                                  .replace(
                                                                    /\//g,
                                                                    "~1",
                                                                  ) +
                                                                "/" +
                                                                key7
                                                                  .replace(
                                                                    /~/g,
                                                                    "~0",
                                                                  )
                                                                  .replace(
                                                                    /\//g,
                                                                    "~1",
                                                                  ) +
                                                                "/outputTokens",
                                                              schemaPath:
                                                                "#/$defs/TokenRateValue/properties/outputTokens/anyOf/1/type",
                                                              keyword: "type",
                                                              params: {
                                                                type: "number",
                                                              },
                                                              message:
                                                                "must be number",
                                                            };
                                                            if (
                                                              vErrors === null
                                                            ) {
                                                              vErrors = [err24];
                                                            } else {
                                                              vErrors.push(
                                                                err24,
                                                              );
                                                            }
                                                            errors++;
                                                          }
                                                        }
                                                        var _valid3 =
                                                          _errs64 === errors;
                                                        valid14 =
                                                          valid14 || _valid3;
                                                        if (!valid14) {
                                                          const err25 = {
                                                            instancePath:
                                                              instancePath +
                                                              "/teams/" +
                                                              key5
                                                                .replace(
                                                                  /~/g,
                                                                  "~0",
                                                                )
                                                                .replace(
                                                                  /\//g,
                                                                  "~1",
                                                                ) +
                                                              "/" +
                                                              key7
                                                                .replace(
                                                                  /~/g,
                                                                  "~0",
                                                                )
                                                                .replace(
                                                                  /\//g,
                                                                  "~1",
                                                                ) +
                                                              "/outputTokens",
                                                            schemaPath:
                                                              "#/$defs/TokenRateValue/properties/outputTokens/anyOf",
                                                            keyword: "anyOf",
                                                            params: {},
                                                            message:
                                                              "must match a schema in anyOf",
                                                          };
                                                          if (
                                                            vErrors === null
                                                          ) {
                                                            vErrors = [err25];
                                                          } else {
                                                            vErrors.push(err25);
                                                          }
                                                          errors++;
                                                          validate25.errors =
                                                            vErrors;
                                                          return false;
                                                        } else {
                                                          errors = _errs61;
                                                          if (
                                                            vErrors !== null
                                                          ) {
                                                            if (_errs61) {
                                                              vErrors.length =
                                                                _errs61;
                                                            } else {
                                                              vErrors = null;
                                                            }
                                                          }
                                                        }
                                                        var valid12 =
                                                          _errs60 === errors;
                                                      } else {
                                                        var valid12 = true;
                                                      }
                                                    }
                                                  }
                                                }
                                              }
                                            }
                                          }
                                        } else {
                                          validate25.errors = [
                                            {
                                              instancePath:
                                                instancePath +
                                                "/teams/" +
                                                key5
                                                  .replace(/~/g, "~0")
                                                  .replace(/\//g, "~1") +
                                                "/" +
                                                key7
                                                  .replace(/~/g, "~0")
                                                  .replace(/\//g, "~1"),
                                              schemaPath:
                                                "#/$defs/TokenRateValue/type",
                                              keyword: "type",
                                              params: { type: "object" },
                                              message: "must be object",
                                            },
                                          ];
                                          return false;
                                        }
                                      }
                                      var valid10 = _errs44 === errors;
                                      if (!valid10) {
                                        break;
                                      }
                                    }
                                  }
                                } else {
                                  validate25.errors = [
                                    {
                                      instancePath:
                                        instancePath +
                                        "/teams/" +
                                        key5
                                          .replace(/~/g, "~0")
                                          .replace(/\//g, "~1"),
                                      schemaPath:
                                        "#/properties/teams/additionalProperties/type",
                                      keyword: "type",
                                      params: { type: "object" },
                                      message: "must be object",
                                    },
                                  ];
                                  return false;
                                }
                              }
                              var valid8 = _errs40 === errors;
                              if (!valid8) {
                                break;
                              }
                            }
                          }
                        } else {
                          validate25.errors = [
                            {
                              instancePath: instancePath + "/teams",
                              schemaPath: "#/properties/teams/type",
                              keyword: "type",
                              params: { type: "object" },
                              message: "must be object",
                            },
                          ];
                          return false;
                        }
                      }
                      var valid0 = _errs36 === errors;
                    } else {
                      var valid0 = true;
                    }
                  }
                }
              }
            }
          }
        }
      }
    } else {
      validate25.errors = [
        {
          instancePath,
          schemaPath: "#/type",
          keyword: "type",
          params: { type: "object" },
          message: "must be object",
        },
      ];
      return false;
    }
  }
  validate25.errors = vErrors;
  return errors === 0;
}
validate25.evaluated = {
  props: true,
  dynamicProps: false,
  dynamicItems: false,
};
export {
  isResourceChangeEvent,
  isResourceHeartbeatEvent,
  isResourceRef,
  isResourceTokenRatesEvent,
};
