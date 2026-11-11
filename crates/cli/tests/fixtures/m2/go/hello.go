// SPDX-License-Identifier: Apache-2.0
// Self-authored M2 fixture.
package main

import "fmt"

type Worker struct{}

func (w Worker) Process(items []int) int {
	return len(items)
}

func main() {
	w := Worker{}
	fmt.Println(w.Process([]int{1, 2}))
}
