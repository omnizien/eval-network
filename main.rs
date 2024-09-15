use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Serialize, Deserialize};
use ring::signature::{KeyPair, Ed25519KeyPair};
use tokio::net::{TcpListener, TcpStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use rand::Rng;
use tokio_tungstenite::{accept_async, tungstenite::protocol::Message};
use futures_util::StreamExt; // Removed SinkExt as before
use ring::rand::SystemRandom;
use thiserror::Error;
use clap::Parser;

// Import the correct Error type
use tokio_tungstenite::tungstenite::Error as TungsteniteError;

// Define constant for the miner wallet address
const MINER_WALLET: &str = "MinerWalletAddress";  // Address for the miner wallet that collects fees

// Custom error type for better error handling
#[derive(Error, Debug)]
pub enum BlockchainError {
    #[error("IO Error occurred: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Serde JSON Error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),
    #[error("Ring Crypto Error: {0}")]
    RingError(String),
    #[error("WebSocket Error: {0}")]
    WebSocketError(#[from] TungsteniteError),
    #[error("Invalid Peer Address")]
    InvalidPeerAddress,
    #[error("Blockchain Validation Failed")]
    BlockchainInvalid,
    #[error("Transaction Error: {0}")]
    TransactionError(String),
}

// Function to generate a new wallet (Ed25519 key pair)
fn generate_wallet() -> Result<String, BlockchainError> {
    // Generate a new key pair (Ed25519) using SystemRandom from ring
    let rng = SystemRandom::new();
    let pkcs8_bytes = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|e| BlockchainError::RingError(format!("Key generation error: {:?}", e)))?;
    let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8_bytes.as_ref())
        .map_err(|e| BlockchainError::RingError(format!("Key pair error: {:?}", e)))?;

    // Return the public key as the wallet address (hex string)
    Ok(hex::encode(key_pair.public_key().as_ref()))
}

// Node struct represents a network node that manages a blockchain and connects to peers
#[derive(Clone)]
struct Node {
    blockchain: Blockchain,          // The node's local blockchain
    peers: Vec<SocketAddr>,          // List of connected peer nodes
}

impl Node {
    // Constructor function to create a new node
    pub fn new() -> Node {
        Node {
            blockchain: Blockchain::new(), // Create a new blockchain for the node
            peers: Vec::new(),             // Initialize an empty list of peers
        }
    }

    // Function to start a listener on a specified port
    pub async fn start_listener(&self, port: u16) -> Result<u16, BlockchainError> {
        let listening_port = if port == 0 {
            let mut rng = rand::thread_rng();
            rng.gen_range(1024..65535) // Random port
        } else {
            port // Use specified port
        };

        let addr = format!("127.0.0.1:{}", listening_port); // Create the address
        let listener = TcpListener::bind(&addr).await.map_err(BlockchainError::IoError)?; // Bind the listener
        println!("Listening on: {}", addr); // Print the listening address

        let node = self.clone(); // Clone self to move into async block

        // Spawn a task to handle incoming connections
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, peer_addr)) => {
                        println!("New connection from: {}", peer_addr);

                        // Handle the connection in a separate task
                        let node_clone = node.clone();
                        tokio::spawn(async move {
                            if let Err(e) = Node::handle_peer_connection(socket, node_clone).await {
                                eprintln!("Error handling peer connection: {}", e);
                            }
                        });
                    }
                    Err(e) => eprintln!("Failed to accept connection: {}", e),
                }
            }
        });

        Ok(listening_port) // Return the port number
    }

    // Function to handle incoming peer connections
    async fn handle_peer_connection(mut socket: TcpStream, node: Node) -> Result<(), BlockchainError> {
        let mut buf = vec![0; 1024];
        match socket.read(&mut buf).await {
            Ok(size) => {
                if size > 0 {
                    let data = String::from_utf8_lossy(&buf[..size]);
                    match serde_json::from_str::<Block>(&data) {
                        Ok(block) => {
                            println!("Received block: {:?}", block);
                            let mut node_blockchain = node.blockchain.clone();
                            node_blockchain.add_block_from_peer(block)?;
                        }
                        Err(_) => {
                            println!("Received data that is not a block");
                        }
                    }
                }
            }
            Err(e) => eprintln!("Failed to read from socket: {}", e),
        }
        Ok(())
    }

    // Function to connect to a peer at the specified address
    pub async fn connect_to_peer(&mut self, addr: SocketAddr) {
        match TcpStream::connect(addr).await {
            Ok(mut stream) => {
                println!("Connected to peer: {}", addr);

                // Add the peer to the list if not already added
                if !self.peers.contains(&addr) {
                    self.peers.push(addr);
                }

                // Serialize the blockchain and send it to the peer
                if let Ok(blockchain_data) = serde_json::to_string(&self.blockchain) {
                    if let Err(e) = stream.write_all(blockchain_data.as_bytes()).await {
                        eprintln!("Failed to send data to peer {}: {}", addr, e);
                    }
                } else {
                    eprintln!("Failed to serialize blockchain data.");
                }
            }
            Err(e) => {
                eprintln!("Failed to connect to peer {}: {}", addr, e);
                // Optionally, you can attempt to reconnect after some time
            }
        }
    }

    // Function to broadcast a block to all connected peers
    pub async fn broadcast_block(&self, block: &Block) {
        let block_data = serde_json::to_string(block).unwrap(); // Serialize the block to JSON
        for peer in &self.peers {
            match TcpStream::connect(peer).await {
                Ok(mut stream) => {
                    if let Err(e) = stream.write_all(block_data.as_bytes()).await {
                        eprintln!("Failed to send block to {}: {}", peer, e);
                    } else {
                        println!("Broadcasted block to peer: {}", peer);
                    }
                }
                Err(e) => eprintln!("Failed to connect to peer {}: {}", peer, e),
            }
        }
    }

    // WebSocket listener for receiving transactions dynamically
    pub async fn start_websocket_listener(addr: &str, node: Arc<Mutex<Node>>) -> Result<(), BlockchainError> {
        let listener = TcpListener::bind(addr).await.map_err(BlockchainError::IoError)?;
        println!("WebSocket listening on: {}", addr);

        while let Ok((stream, _)) = listener.accept().await {
            let node_clone = Arc::clone(&node);
            tokio::spawn(async move {
                if let Err(e) = handle_transaction_connection(stream, node_clone).await {
                    eprintln!("Error handling transaction connection: {}", e);
                }
            });
        }
        Ok(())
    }
}

// Function to handle WebSocket connections and transactions
async fn handle_transaction_connection(stream: TcpStream, node: Arc<Mutex<Node>>) -> Result<(), BlockchainError> {
    let ws_stream = accept_async(stream).await?;
    let (_write, mut read) = ws_stream.split();

    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                println!("Received a transaction: {}", text);
                match serde_json::from_str::<Transaction>(&text) {
                    Ok(transaction) => {
                        println!("Transaction received: {:?}", transaction);

                        // Lock the node to add the transaction to the blockchain
                        let mut node_lock = node.lock().await;
                        node_lock.blockchain.add_transaction(transaction)?;

                        // Optionally mine a new block after receiving enough transactions
                        let new_block_option = node_lock.blockchain.mine_pending_transactions().await?;

                        // Drop the mutable borrow of node_lock
                        drop(node_lock);

                        if let Some(new_block) = new_block_option {
                            // Now we can lock the node again to broadcast the block
                            let node_clone = Arc::clone(&node);
                            tokio::spawn(async move {
                                let node_lock = node_clone.lock().await;
                                node_lock.broadcast_block(&new_block).await;
                            });
                        }
                    }
                    Err(e) => eprintln!("Failed to deserialize transaction: {}", e),
                }
            },
            Ok(_) => println!("Non-text message received, ignoring."),
            Err(e) => eprintln!("WebSocket error: {}", e),
        }
    }
    Ok(())
}

// Transaction struct that represents a transaction in the blockchain
#[derive(Debug, Serialize, Deserialize, Clone)]
struct Transaction {
    sender: String,       // Address or identity of the sender
    receiver: String,     // Address or identity of the receiver
    amount: f32,          // Amount to be transferred to the receiver
    fee: f32,             // The fee that goes to the miner
    signature: String,    // Digital signature to verify the authenticity of the transaction
}

// Block struct represents a block in the blockchain
#[derive(Debug, Serialize, Deserialize, Clone)]
struct Block {
    index: u32,                      // Block index (position in the blockchain)
    timestamp: u128,                 // Timestamp when the block was created
    transactions: Vec<Transaction>,  // List of transactions included in this block
    previous_hash: String,           // Hash of the previous block in the chain
    hash: String,                    // Hash of this block (calculated after mining)
    nonce: u64,                      // A counter used for mining (Proof of Work)
    miner_wallet: String,            // The wallet that receives the fees for mining this block
}

impl Block {
    fn new(
        index: u32,
        transactions: Vec<Transaction>,
        previous_hash: String,
        miner_wallet: String,
    ) -> Result<Block, BlockchainError> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| BlockchainError::IoError(std::io::Error::new(std::io::ErrorKind::Other, e)))?
            .as_millis();

        let mut block = Block {
            index,
            timestamp,
            transactions,
            previous_hash,
            hash: String::new(),
            nonce: 0,
            miner_wallet,
        };

        block.mine_block(2); // Start mining process with a difficulty of 2
        Ok(block)
    }

    fn calculate_hash(&self) -> String {
        let mut hasher = Sha256::new();
        let input = format!(
            "{}{}{:?}{}{}{}",
            self.index, self.timestamp, self.transactions, self.previous_hash, self.nonce, self.miner_wallet
        );
        hasher.update(input.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    fn mine_block(&mut self, difficulty: usize) {
        let target = "0".repeat(difficulty);
        self.hash = self.calculate_hash();

        while &self.hash[..difficulty] != target {
            self.nonce += 1;
            self.hash = self.calculate_hash();
        }

        println!("Block mined: {}", self.hash);
    }
}

// Blockchain struct that manages the chain of blocks and the transaction pool
#[derive(Debug, Serialize, Deserialize, Clone)]
struct Blockchain {
    chain: Vec<Block>,
    transaction_pool: Vec<Transaction>,
    fee_pool: f32,
}

impl Blockchain {
    fn new() -> Blockchain {
        Blockchain {
            chain: vec![Blockchain::create_genesis_block()],
            transaction_pool: vec![],
            fee_pool: 0.0,
        }
    }

    fn create_genesis_block() -> Block {
        Block::new(0, vec![], "0".to_string(), MINER_WALLET.to_string())
            .expect("Failed to create genesis block")
    }

    // Function to mine pending transactions into a new block
    async fn mine_pending_transactions(&mut self) -> Result<Option<Block>, BlockchainError> {
        if self.transaction_pool.is_empty() {
            println!("No transactions to mine");
            return Ok(None);
        }

        let previous_block = self.chain.last().unwrap();

        // Reward transaction for the miner
        let fee_transaction = Transaction {
            sender: "Network".to_string(),
            receiver: MINER_WALLET.to_string(),
            amount: self.fee_pool,
            fee: 0.0,
            signature: "Reward".to_string(),
        };

        let mut transactions = self.transaction_pool.clone();
        transactions.push(fee_transaction);

        let new_block = Block::new(
            self.chain.len() as u32,
            transactions,
            previous_block.hash.clone(),
            MINER_WALLET.to_string(),
        )?;

        self.chain.push(new_block.clone());
        self.transaction_pool.clear();
        self.fee_pool = 0.0;

        // Return the new block
        Ok(Some(new_block))
    }

    // Function to add a block received from a peer
    fn add_block_from_peer(&mut self, block: Block) -> Result<(), BlockchainError> {
        // Validate the block before adding
        if let Some(previous_block) = self.chain.last() {
            if block.previous_hash != previous_block.hash {
                return Err(BlockchainError::BlockchainInvalid);
            }

            if block.hash != block.calculate_hash() {
                return Err(BlockchainError::BlockchainInvalid);
            }

            self.chain.push(block);
            println!("Block added to the blockchain from peer");
        } else {
            return Err(BlockchainError::BlockchainInvalid);
        }
        Ok(())
    }

    fn add_transaction(&mut self, transaction: Transaction) -> Result<(), BlockchainError> {
        // Here you could add transaction verification logic
        self.fee_pool += transaction.fee;
        self.transaction_pool.push(transaction);
        Ok(())
    }
}

// Command-line arguments struct using clap
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Port number for the node to listen on
    #[clap(short = 'o', long, default_value = "0")]
    port: u16,

    /// Peer addresses to connect to
    #[clap(short = 'p', long)]
    peers: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), BlockchainError> {
    // Parse command-line arguments
    let args = Args::parse();

    // Example usage: create a new block
    let miner_wallet = None; // This could be None, meaning no wallet was provided
    let transactions = vec![]; // Placeholder transactions
    let previous_hash = String::from("0000");

    // If miner_wallet is None, use a default value or generate a new wallet
    let wallet = match miner_wallet {
        Some(wallet) => wallet,
        None => {
            println!("No wallet provided, generating a new one...");
            generate_wallet()?
        }
    };

    // Create a new block; pass the `wallet` (String) to the Block::new function
    let new_block = Block::new(1, transactions, previous_hash, wallet.clone())?;

    println!("Miner wallet address: {}", new_block.miner_wallet);

    // Create a new node and wrap it in an Arc<Mutex<>> for shared, thread-safe access
    let node = Arc::new(Mutex::new(Node::new()));

    // Start a listener for the node on the specified port
    let listening_port;
    {
        let node_lock = node.lock().await;
        listening_port = node_lock.start_listener(args.port).await?;
    }

    println!("Node is listening on port: {}", listening_port);

    // Spawn a WebSocket listener on a specific port (e.g., listening_port + 1)
    let node_for_websocket = Arc::clone(&node);
    let ws_port = listening_port + 1;
    let ws_addr = format!("127.0.0.1:{}", ws_port);

    // Move the println! before the tokio::spawn
    println!("WebSocket listening on: {}", ws_addr);

    tokio::spawn(async move {
        if let Err(e) = Node::start_websocket_listener(&ws_addr, node_for_websocket).await {
            eprintln!("WebSocket listener error: {}", e);
        }
    });

    // Connect to specified peers
    if !args.peers.is_empty() {
        let mut node_lock = node.lock().await;
        for peer in args.peers {
            match peer.parse::<SocketAddr>() {
                Ok(peer_addr) => {
                    node_lock.connect_to_peer(peer_addr).await;
                }
                Err(_) => eprintln!("Invalid peer address: {}", peer),
            }
        }
    }

    // Main loop to keep the node alive, allowing time for transactions to be processed
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
    }
}
