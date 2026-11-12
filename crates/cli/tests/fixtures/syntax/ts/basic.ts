// SPDX-License-Identifier: Apache-2.0
import { a } from "./a";
export interface I { m(): void }
type Alias = string | number;
enum E { X }
export class C implements I {
  private n = 1;
  constructor() { a(); }
  m(): void { this.helper(); }
  helper = () => { a(); };
}
export function f<T>(x: T): T { return x; }
const g = function () { f(1); };
namespace N { export function inner() {} }
abstract class AC {}
