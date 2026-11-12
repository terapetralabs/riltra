// SPDX-License-Identifier: Apache-2.0
#include <vector>
namespace ns { void free_fn() {} }
class Base { public: virtual void f(); int field; };
struct D : Base { void f() override { ns::free_fn(); } };
template <typename T> T twice(T v) { return v; }
void Base::f() {}
enum class K { A };
using IntVec = std::vector<int>;
int main() { D d; d.f(); twice<int>(1); auto l = [] { return 1; }; return l(); }
class Fwd; struct Fs; Fs *fp;
