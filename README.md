# Little Loadshedder
[![Crates.io](https://img.shields.io/crates/v/little-loadshedder.svg)](https://crates.io/crates/little-loadshedder)
[![API reference](https://docs.rs/little-loadshedder/badge.svg)](https://docs.rs/little-loadshedder/)
[![License](https://img.shields.io/badge/license-MIT_OR_Apache--2.0-blue.svg)](
https://github.com/Skepfyr/little-loadshedder#license)

A Rust hyper/tower service that implements load shedding with queuing & concurrency limiting based on latency.

It uses [Little's Law](https://en.wikipedia.org/wiki/Little%27s_law) to intelligently shed load in order to maintain a target average latency.
It achieves this by placing a queue in front of the service it wraps, the size of which is determined by measuring the average latency of calls to the inner service.
Additionally, it controls the number of concurrent requests to the inner service, in order to achieve the maximum possible throughput.

The following images show metrics from the example server under load generated by the example client.

First, when load is turned on, some requests are rejected while the middleware works out the queue size, and increases the concurrency.
![startup](https://user-images.githubusercontent.com/3080863/215579375-39abe718-2820-4fb4-ac52-3a5ca705085d.png)
This quickly resolves to a steady state where the service can easily handle the load on it, and so the queue size is large.

Next, we send a large burst of traffic, note that none of it is dropped as it is all absorbed by the queue.
![burst](https://user-images.githubusercontent.com/3080863/215579359-075c94fe-245b-47fb-899e-d273b954bb1e.png)
Once the burst stops, the service slowly clears it's backlog of requests and returns to the steady state.

Now we simulate a service degradation, all requests are taking twice as long to process.
The queue shrinks to about half its original size in order to hit the target average latency, however the service cannot acheive this throughput any longer so the queue fills and requests are rejected.
![degradation](https://user-images.githubusercontent.com/3080863/215579365-7af527c9-745f-4ab4-b87f-813a09664018.png)
Note that from the client's point of view requests are either immediately rejected or complete at roughly the target latency.

Now the service degrades substantially. The queue shrinks to almost nothing and the concurrency is slowly reduced until the latency matches the goal.
![slow](https://user-images.githubusercontent.com/3080863/215579369-1f1261ec-8029-49e1-817a-a77f47e2f747.png)

Finally, the service recovers, the middleware rapidly notices and returns to it's inital steady state.
![recovery](https://user-images.githubusercontent.com/3080863/215579367-17a94de3-674c-4e3b-84a3-185daf087956.png)

## License
Licensed under either of  

* Apache License, Version 2.0
  ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
* MIT license
  ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contribution
This project welcomes contributions and suggestions, just open an issue or pull request!

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

