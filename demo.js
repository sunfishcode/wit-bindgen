import { data_view, to_string, UTF8_DECODER, utf8_encode, UTF8_ENCODED_LEN, Slab } from './intrinsics.js';
export class Demo {
  constructor() {
    this._resource0_slab = new Slab();
  }
  addToImports(imports) {
    if (!("canonical_abi" in imports)) imports["canonical_abi"] = {};
    
    imports.canonical_abi['resource_drop_config'] = i => {
      this._resource0_slab.remove(i).drop();
    };
    imports.canonical_abi['resource_clone_config'] = i => {
      const obj = this._resource0_slab.get(i);
      return this._resource0_slab.insert(obj.clone())
    };
    imports.canonical_abi['resource_get_config'] = i => {
      return this._resource0_slab.get(i)._wasm_val;
    };
    imports.canonical_abi['resource_new_config'] = i => {
      const registry = this._registry0;
      return this._resource0_slab.insert(new Config(i, this));
    };
  }
  
  async instantiate(module, imports) {
    imports = imports || {};
    this.addToImports(imports);
    
    if (module instanceof WebAssembly.Instance) {
      this.instance = module;
    } else if (module instanceof WebAssembly.Module) {
      this.instance = await WebAssembly.instantiate(module, imports);
    } else if (module instanceof ArrayBuffer || module instanceof Uint8Array) {
      const { instance } = await WebAssembly.instantiate(module, imports);
      this.instance = instance;
    } else {
      const { instance } = await WebAssembly.instantiateStreaming(module, imports);
      this.instance = instance;
    }
    this._exports = this.instance.exports;
    this._registry0 = new FinalizationRegistry(this._exports['canonical_abi_drop_config']);
  }
}

export class Config {
  constructor(wasm_val, obj) {
    this._wasm_val = wasm_val;
    this._obj = obj;
    this._refcnt = 1;
    obj._registry0.register(this, wasm_val, this);
  }
  
  clone() {
    this._refcnt += 1;
    return this;
  }
  
  drop() {
    this._refcnt -= 1;
    if (this._refcnt !== 0)
    return;
    this._obj._registry0.unregister(this);
    const dtor = this._obj._exports['canonical_abi_drop_config'];
    const wasm_val = this._wasm_val;
    delete this._obj;
    delete this._refcnt;
    delete this._wasm_val;
    dtor(wasm_val);
  }
  static new(demo) {
    const ret = demo._exports['config::new']();
    return demo._resource0_slab.remove(ret);
  }
  render(arg1, arg2, arg3) {
    const memory = this._obj._exports.memory;
    const realloc = this._obj._exports["cabi_realloc"];
    const obj0 = this;
    const val1 = to_string(arg1);
    let enum1;
    switch (val1) {
      case "js": {
        enum1 = 0;
        break;
      }
      case "rust": {
        enum1 = 1;
        break;
      }
      case "java": {
        enum1 = 2;
        break;
      }
      case "wasmtime": {
        enum1 = 3;
        break;
      }
      case "wasmtime-py": {
        enum1 = 4;
        break;
      }
      case "c": {
        enum1 = 5;
        break;
      }
      case "markdown": {
        enum1 = 6;
        break;
      }
      default: {
        throw new TypeError(`"${val1}" is not one of the cases of lang`);
      }
    }
    const ptr2 = utf8_encode(arg2, realloc, memory);
    const len2 = UTF8_ENCODED_LEN;
    const ret = this._obj._exports['config::render'](this._obj._resource0_slab.insert(obj0.clone()), enum1, ptr2, len2, arg3 ? 1 : 0);
    
    let variant7;
    switch (data_view(memory).getUint8(ret + 0, true)) {
      case 0: {
        const len5 = data_view(memory).getInt32(ret + 8, true);
        const base5 = data_view(memory).getInt32(ret + 4, true);
        const result5 = [];
        for (let i = 0; i < len5; i++) {
          const base = base5 + i * 16;
          const ptr3 = data_view(memory).getInt32(base + 0, true);
          const len3 = data_view(memory).getInt32(base + 4, true);
          const result3 = UTF8_DECODER.decode(new Uint8Array(memory.buffer, ptr3, len3));
          const ptr4 = data_view(memory).getInt32(base + 8, true);
          const len4 = data_view(memory).getInt32(base + 12, true);
          const result4 = UTF8_DECODER.decode(new Uint8Array(memory.buffer, ptr4, len4));
          result5.push([result3, result4]);
        }
        
        variant7 = { tag: "ok", val: result5 };
        break;
      }
      case 1: {
        const ptr6 = data_view(memory).getInt32(ret + 4, true);
        const len6 = data_view(memory).getInt32(ret + 8, true);
        const result6 = UTF8_DECODER.decode(new Uint8Array(memory.buffer, ptr6, len6));
        
        variant7 = { tag: "err", val: result6 };
        break;
      }
      default: {
        throw new RangeError("invalid variant discriminant for expected");
      }
    }
    this._obj._exports["cabi_post_config::render"](ret);
    return variant7;
  }
  setRustUnchecked(arg1) {
    const obj0 = this;
    this._obj._exports['config::set-rust-unchecked'](this._obj._resource0_slab.insert(obj0.clone()), arg1 ? 1 : 0);
  }
  setWasmtimeTracing(arg1) {
    const obj0 = this;
    this._obj._exports['config::set-wasmtime-tracing'](this._obj._resource0_slab.insert(obj0.clone()), arg1 ? 1 : 0);
  }
  setWasmtimeCustomError(arg1) {
    const obj0 = this;
    this._obj._exports['config::set-wasmtime-custom-error'](this._obj._resource0_slab.insert(obj0.clone()), arg1 ? 1 : 0);
  }
}
