# TODO
- Permit Send trait on all structs in the project. (this makes more easy the concurrency and multithreading and development);
- Reduce cache default size to 128MiB;

## Not Urgent
 - Custom Debug trait for all structs in the project. (making possible hidden all information of internals structs of this project);
 - Custom thiserror packege. (same ideia, hidden all information like litteral keys and values, strings use only enums and raw data);
 - Why the object of Auth has enum to be AnonymousUser and inside there's a U48::MAX to means anonymous user? (not more easy to use Option and if is None, mean anonymous user?);
 -  