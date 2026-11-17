package main

func target() int {
	return 1
}

func caller() int {
	return target()
}

func unrelated() int {
	return 2
}
