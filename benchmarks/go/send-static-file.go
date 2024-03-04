package main

import (
	"fmt"
	"net/http"
)

import _ "embed"

//go:embed static/Cargo.toml
var cargoToml string

var workerCount = 2
var workerPool chan struct{}

func main() {
	// Create a buffered channel to limit the number of concurrent worker threads
	workerPool = make(chan struct{}, workerCount)

	// Handle requests using a single IO thread
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		// Acquire a worker thread from the pool
		workerPool <- struct{}{}

		// Handle the request in a goroutine
		{
			// Ensure the worker is released back to the pool when done
			defer func() {
				<-workerPool
			}()

			// Handle the actual request (in this case, just returning "Hello, World!")
			handleRequest(w, r)
		}
	})

	// Start the web server on port 8080
	fmt.Println("Server listening on :8080")
	http.ListenAndServe(":8080", nil)
}

func handleRequest(w http.ResponseWriter, r *http.Request) {
    // Simulate some work being done by the worker
	// fmt.Println("Processing request...")

	// Set the Content-Type header to plain text
	w.Header().Set("Content-Type", "text/plain")

	// Get the response string
	response := cargoToml 

	// Set the Content-Length header
	w.Header().Set("Content-Length", fmt.Sprint(len(response)))

	// Write the response body
	fmt.Fprint(w, response)
}
