// Package server is a fixture.
package server

import "strings"

const DefaultPort = 8080

// Server answers requests.
// It keeps no state.
type Server struct {
	name string
}

type Handler interface {
	Serve(path string) string
}

type Port int

// NewServer builds a server.
func NewServer(name string) *Server {
	return &Server{name: strings.TrimSpace(name)}
}

// Serve answers one path.
func (s *Server) Serve(path string) string {
	return s.name + helper(path)
}

func helper(p string) string {
	return strings.ToUpper(p)
}
