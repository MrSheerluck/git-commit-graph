use flate2::read::ZlibDecoder;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::rc::{Rc, Weak};

struct CommitNode {
    hash: String,
    short_hash: String,
    message: String,
    timestamp: u64,
    parents: RefCell<Vec<Weak<RefCell<CommitNode>>>>,
    children: RefCell<Vec<Weak<RefCell<CommitNode>>>>,
}

#[allow(dead_code)]
struct RawCommit {
    parent_hashes: Vec<String>,
    author: String,
    timestamp: u64,
    message: String,
}

fn read_head(git_dir: &Path) -> Option<String> {
    let content = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    Some(content.trim().to_string())
}

fn resolve_ref(git_dir: &Path, reference: &str) -> Option<String> {
    if reference.starts_with("ref: ") {
        let ref_path = &reference[5..];
        fs::read_to_string(git_dir.join(ref_path))
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        Some(reference.to_string())
    }
}

fn read_all_branches(git_dir: &Path) -> Vec<(String, String)> {
    let mut branches = Vec::new();
    let heads_dir = git_dir.join("refs").join("heads");
    if let Ok(entries) = fs::read_dir(&heads_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if let Ok(hash) = fs::read_to_string(&path) {
                    branches.push((name.to_string(), hash.trim().to_string()));
                }
            }
        }
    }
    branches
}

fn read_object(git_dir: &Path, hash: &str) -> Vec<u8> {
    let object_path = git_dir
        .join("objects")
        .join(&hash[..2])
        .join(&hash[2..]);
    let compressed =
        fs::read(&object_path).unwrap_or_else(|_| panic!("Failed to read object: {}", hash));
    let mut decoder = ZlibDecoder::new(&compressed[..]);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .unwrap_or_else(|_| panic!("Failed to decompress object: {}", hash));
    decompressed
}

fn parse_object(data: &[u8]) -> (&str, &[u8]) {
    let header_end = data
        .iter()
        .position(|&b| b == 0)
        .expect("Invalid object: no null byte");
    let header =
        std::str::from_utf8(&data[..header_end]).expect("Invalid object: header not valid UTF-8");
    let content = &data[header_end + 1..];
    let mut parts = header.splitn(2, ' ');
    let obj_type = parts.next().unwrap();
    let _size = parts.next().unwrap();
    (obj_type, content)
}

fn parse_commit(content: &[u8]) -> RawCommit {
    let content_str =
        std::str::from_utf8(content).expect("Invalid commit: not valid UTF-8");
    let mut parent_hashes = Vec::new();
    let mut author = String::new();
    let mut timestamp: u64 = 0;
    let mut message = String::new();
    let mut in_message = false;

    for line in content_str.lines() {
        if in_message {
            if !message.is_empty() {
                message.push('\n');
            }
            message.push_str(line);
            continue;
        }
        if line.is_empty() {
            in_message = true;
            continue;
        }
        if let Some(hash) = line.strip_prefix("parent ") {
            parent_hashes.push(hash.to_string());
        } else if let Some(author_line) = line.strip_prefix("author ") {
            author = author_line.to_string();
            if let Some(last_space) = author_line.rfind(' ') {
                if let Some(prev_space) = author_line[..last_space].rfind(' ') {
                    let ts_str = &author_line[prev_space + 1..last_space];
                    if let Ok(ts) = ts_str.parse::<u64>() {
                        timestamp = ts;
                    }
                }
            }
        }
    }

    RawCommit {
        parent_hashes,
        author,
        timestamp,
        message,
    }
}

fn read_commit_recursive(
    git_dir: &Path,
    hash: &str,
    raw_commits: &mut HashMap<String, RawCommit>,
) {
    if raw_commits.contains_key(hash) {
        return;
    }
    let data = read_object(git_dir, hash);
    let (obj_type, content) = parse_object(&data);
    match obj_type {
        "commit" => {
            let commit = parse_commit(content);
            for parent in &commit.parent_hashes {
                read_commit_recursive(git_dir, parent, raw_commits);
            }
            raw_commits.insert(hash.to_string(), commit);
        }
        _ => panic!("Expected commit object, got {}", obj_type),
    }
}

fn read_all_commits(git_dir: &Path, head_hash: &str) -> HashMap<String, RawCommit> {
    let mut raw_commits = HashMap::new();
    read_commit_recursive(git_dir, head_hash, &mut raw_commits);
    raw_commits
}

fn build_graph(
    raw_commits: &HashMap<String, RawCommit>,
) -> HashMap<String, Rc<RefCell<CommitNode>>> {
    let mut nodes: HashMap<String, Rc<RefCell<CommitNode>>> = HashMap::new();

    for (hash, raw) in raw_commits {
        let node = Rc::new(RefCell::new(CommitNode {
            hash: hash.clone(),
            short_hash: hash[..7].to_string(),
            message: raw.message.lines().next().unwrap_or("").to_string(),
            timestamp: raw.timestamp,
            parents: RefCell::new(Vec::new()),
            children: RefCell::new(Vec::new()),
        }));
        nodes.insert(hash.clone(), node);
    }

    nodes
}

fn wire_edges(
    nodes: &HashMap<String, Rc<RefCell<CommitNode>>>,
    raw_commits: &HashMap<String, RawCommit>,
) {
    for (hash, node) in nodes {
        if let Some(raw) = raw_commits.get(hash) {
            for parent_hash in &raw.parent_hashes {
                if let Some(parent_node) = nodes.get(parent_hash) {
                    node.borrow()
                        .parents
                        .borrow_mut()
                        .push(Rc::downgrade(parent_node));

                    parent_node
                        .borrow()
                        .children
                        .borrow_mut()
                        .push(Rc::downgrade(node));
                }
            }
        }
    }
}

fn walk_first_parent_chain(
    nodes: &HashMap<String, Rc<RefCell<CommitNode>>>,
    start_hash: &str,
) -> Vec<String> {
    let mut path = Vec::new();
    let mut current_hash = start_hash.to_string();

    loop {
        if let Some(node) = nodes.get(&current_hash) {
            let n = node.borrow();
            path.push(current_hash.clone());

            match n.parents.borrow().first() {
                Some(first_parent) => match first_parent.upgrade() {
                    Some(parent) => current_hash = parent.borrow().hash.clone(),
                    None => break,
                },
                None => break,
            }
        } else {
            break;
        }
    }

    path
}

fn print_refcounts(nodes: &HashMap<String, Rc<RefCell<CommitNode>>>) {
    println!("Reference counts (nodes with at least one weak edge):");
    for (hash, node) in nodes {
        let weak = Rc::weak_count(node);
        if weak > 0 {
            let strong = Rc::strong_count(node);
            println!(
                "  {} has {} strong, {} weak references",
                &hash[..7], strong, weak
            );
        }
    }
}

fn render_graph(
    nodes: &HashMap<String, Rc<RefCell<CommitNode>>>,
    head_hash: &str,
    branches: &[(String, String)],
) {
    let branch_map: HashMap<&str, &str> = branches
        .iter()
        .map(|(name, hash)| (hash.as_str(), name.as_str()))
        .collect();

    let main_line = walk_first_parent_chain(nodes, head_hash);
    let main_set: HashSet<&str> =
        main_line.iter().map(|h| h.as_str()).collect();
    let mut shown: HashMap<String, bool> = HashMap::new();

    for hash in &main_line {
        if let Some(node) = nodes.get(hash) {
            let n = node.borrow();
            shown.insert(hash.clone(), true);

            let parents: Vec<Rc<RefCell<CommitNode>>> = n
                .parents
                .borrow()
                .iter()
                .filter_map(|w| w.upgrade())
                .collect();

            if parents.len() > 1 {
                for parent in parents.iter().skip(1) {
                    let p = parent.borrow();
                    let side_line = walk_first_parent_chain(nodes, &p.hash);

                    println!("|\\");
                    for side_hash in &side_line {
                        if main_set.contains(side_hash.as_str()) {
                            break;
                        }
                        let label = branch_map
                            .get(side_hash.as_str())
                            .map(|name| format!(" ({})", name))
                            .unwrap_or_default();

                        if let Some(side_node) = nodes.get(side_hash) {
                            let sn = side_node.borrow();
                            println!("| * {} {}{}", sn.short_hash, sn.message, label);
                            shown.insert(side_hash.clone(), true);
                        }
                    }
                    println!("|/");
                }
            }

            let label = branch_map
                .get(hash.as_str())
                .map(|name| format!(" ({})", name))
                .unwrap_or_default();

            println!("* {} {}{}", n.short_hash, n.message, label);
        }
    }

    for (branch_name, branch_hash) in branches {
        if !shown.contains_key(branch_hash.as_str()) {
            if let Some(_node) = nodes.get(branch_hash.as_str()) {
                let side_line = walk_first_parent_chain(nodes, branch_hash);
                println!("\n  (unconnected branch '{}')", branch_name);
                for side_hash in &side_line {
                    if let Some(side_node) = nodes.get(side_hash) {
                        let sn = side_node.borrow();
                        println!("    * {} {}", sn.short_hash, sn.message);
                    }
                    if shown.contains_key(side_hash) {
                        break;
                    }
                }
            }
        }
    }

    println!("\n{}", "-".repeat(50));
    println!("Graph statistics:");
    println!("  Total commits: {}", nodes.len());
    println!("  Branch refs:    {}", branches.len());

    let merge_count = nodes
        .values()
        .filter(|node| node.borrow().parents.borrow().len() > 1)
        .count();
    println!("  Merge commits:  {}", merge_count);

    let root_count = nodes
        .values()
        .filter(|node| node.borrow().parents.borrow().is_empty())
        .count();
    println!("  Root commits:   {}", root_count);

    let timestamps: Vec<u64> = nodes
        .values()
        .map(|node| node.borrow().timestamp)
        .collect();
    if let (Some(min), Some(max)) = (timestamps.iter().min(), timestamps.iter().max()) {
        println!("  Oldest commit:  {} (Unix epoch)", min);
        println!("  Newest commit:  {} (Unix epoch)", max);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let git_dir = if args.len() > 1 {
        Path::new(&args[1]).to_path_buf()
    } else {
        Path::new(".git").to_path_buf()
    };

    let head = match read_head(&git_dir) {
        Some(h) => h,
        None => {
            eprintln!("Error: {} is not a git repository (no HEAD file)", git_dir.display());
            return;
        }
    };
    let head_hash = match resolve_ref(&git_dir, &head) {
        Some(h) => h,
        None => {
            eprintln!("Error: failed to resolve HEAD reference '{}'", head);
            return;
        }
    };
    let branches = read_all_branches(&git_dir);

    println!("Reading commits from {}...", git_dir.display());
    let raw_commits = read_all_commits(&git_dir, &head_hash);
    println!("Parsed {} commits.\n", raw_commits.len());

    let graph = build_graph(&raw_commits);
    wire_edges(&graph, &raw_commits);

    print_refcounts(&graph);

    println!("\nCommit Graph:\n");
    render_graph(&graph, &head_hash, &branches);
}
