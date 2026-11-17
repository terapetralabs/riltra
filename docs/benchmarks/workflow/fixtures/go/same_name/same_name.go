package main

type A struct{}

type B struct{}

func (a A) run() int {
	return 1
}

func (b B) run() int {
	return 2
}

func useA(a A) int {
	return a.run()
}

func useB(b B) int {
	return b.run()
}
