// SPDX-License-Identifier: Apache-2.0
package calls

func a() {
	F[int](1)
	pkg.F[int](1, 2)
	G[int, string](1, 2)
	T(3)
	_ = M[int]
}
