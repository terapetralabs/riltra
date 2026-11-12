// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;
mod inner { pub fn deep() {} }
struct S { n: i32 }
enum E { A, B }
trait T { fn t(&self); fn d(&self) { helper(); } }
impl S {
    fn new() -> S { S { n: 0 } }
    fn run(&self) { self.go(); inner::deep(); println!("x"); }
    fn go(&self) {}
}
type Alias = HashMap<String, i32>;
fn helper() { let v = Vec::new(); v.push(1); }
const C: i32 = 1;
