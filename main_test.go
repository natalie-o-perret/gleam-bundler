package main

import (
	"bytes"
	"testing"
)

func TestPackageName(t *testing.T) {
	name, err := packageName("name = \"hello_world\" # comment\n[dependencies]\n")
	if err != nil || name != "hello_world" {
		t.Fatalf("packageName() = %q, %v", name, err)
	}
	name, err = packageName("name='tiny2'\n")
	if err != nil || name != "tiny2" {
		t.Fatalf("packageName() = %q, %v", name, err)
	}
	if _, err := packageName("name = \"unterminated\n"); err == nil {
		t.Fatal("packageName() accepted an unterminated name")
	}
	if _, err := packageName("[dependencies]\nname = \"wrong_table\"\n"); err == nil {
		t.Fatal("packageName() accepted a name outside the top level")
	}
}

func TestReadFooter(t *testing.T) {
	payload := []byte("payload")
	want := footer{offset: 3, length: uint64(len(payload)), hash: 42}
	executable := append([]byte("exe"), payload...)
	executable = append(executable, want.encode()...)

	got, found, err := readFooter(bytes.NewReader(executable))
	if err != nil || !found || got != want {
		t.Fatalf("readFooter() = %#v, %v, %v", got, found, err)
	}
}

func TestHashRange(t *testing.T) {
	got, err := hashRange(bytes.NewReader([]byte("xhelloy")), 1, 5)
	if err != nil {
		t.Fatal(err)
	}
	if got != 0xa430d84680aabd0b {
		t.Fatalf("hashRange() = %#x", got)
	}
}

func TestOutputFilename(t *testing.T) {
	for _, name := range []string{".", "..", "path/.", "path/..", "/"} {
		if hasOutputFilename(name) {
			t.Errorf("hasOutputFilename(%q) = true", name)
		}
	}
	for _, name := range []string{"bundle", "path/bundle", "path/bundle/"} {
		if !hasOutputFilename(name) {
			t.Errorf("hasOutputFilename(%q) = false", name)
		}
	}
}
