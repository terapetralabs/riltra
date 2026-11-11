// SPDX-License-Identifier: Apache-2.0
package main

import (
	"fmt"
	str "strings"
)

type Server struct{ name string }

type Handler interface{ Serve() }

func helper(n int) int { return n + 1 }

func (s *Server) Start() {
	fmt.Println(helper(1))
	str.ToUpper(s.name)
}

func main() {
	s := &Server{name: "x"}
	s.Start()
}
