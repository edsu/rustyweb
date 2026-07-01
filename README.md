# rustyweb

rustyweb is a web archive player that combines the high-fidelity browser based replay of ReplayWebPage with the performance of Rust on the server side. The server essentially lets you:

* index WARC and WACZ data
* serves up the ReplayWeb page client
* provides a CDX API for ReplayWeb page to talk to
* provides a search interface API for doing full text search across the data

The rustyweb executable can be run as a web server. But also includes a command line for indexing WARC and WACZ data.
