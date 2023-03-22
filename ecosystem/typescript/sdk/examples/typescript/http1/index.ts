import { TxnBuilderTypes, BCS, AptosAccount, AptosClient, FaucetClient } from "../../../dist";
import { exit } from "process";
import { BatchTransaction } from "./batch_transactions";
import { Timer } from "timer-node";

async function main() {
  const faucetClient = new FaucetClient("http://0.0.0.0:8080/v1", "http://0.0.0.0:8081");
  //const faucetClient = new FaucetClient("https://fullnode.devnet.aptoslabs.com", "https://faucet.devnet.aptoslabs.com");

  const account1 = new AptosAccount();
  await faucetClient.fundAccount(account1.address(), 100_000_000);
  const account2 = new AptosAccount();

  const entryFunctionPayload = new TxnBuilderTypes.TransactionPayloadEntryFunction(
    TxnBuilderTypes.EntryFunction.natural(
      "0x1::aptos_account",
      "transfer",
      [],
      [BCS.bcsToBytes(TxnBuilderTypes.AccountAddress.fromHex(account2.address())), BCS.bcsSerializeUint64(1)],
    ),
  );

  console.log("/////submitting transaction with http1 for", account1.address().hex());
  const batch = new BatchTransaction(account1, entryFunctionPayload, {
    maxGasAmount: BigInt(200000),
    gasUnitPrice: BigInt(100),
  });
  const timer = new Timer();
  // timer.start();
  // let transactions: Uint8Array[] = [];
  // for (let i = 0; i < 50; i++) {
  //   const txn = await batch.generateBscTxn();
  //   transactions.push(txn!);
  // }
  // const result = await batch.send(transactions);
  // console.log("result", result);

  const paths: string[] = [];
  for (let i = 0; i < 500; i++) {
    paths.push(`/v1/accounts/${account1.address().hex()}`);
  }
  const data = await batch.get(paths).catch((err: any) => {
    console.error(err);
  });
  // console.log(data);
  timer.stop();
  console.log(timer.time());
  exit(0);
}

main();
