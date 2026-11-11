// SPDX-License-Identifier: Apache-2.0
package gen

import "os"

type Stack[T any] struct{ items []T }

func (s *Stack[T]) Push(v T) { s.items = append(s.items, v) }

func Map[T, U any](xs []T, f func(T) U) []U {
	out := make([]U, 0, len(xs))
	for _, x := range xs {
		out = append(out, f(x))
	}
	return out
}

func run() {
	defer os.Exit(0)
	go Map([]int{1}, func(i int) string { return "" })
}
