

@file:Suppress("RemoveRedundantBackticks")

package org.lightningdevkit.ldknode

// Common helper code.
//
// Ideally this would live in a separate .kt file where it can be unittested etc
// in isolation, and perhaps even published as a re-useable package.
//
// However, it's important that the details of how this helper code works (e.g. the
// way that different builtin types are passed across the FFI) exactly match what's
// expected by the Rust code on the other side of the interface. In practice right
// now that means coming from the exact some version of `uniffi` that was used to
// compile the Rust component. The easiest way to ensure this is to bundle the Kotlin
// helpers directly inline like we're doing here.

public class InternalException(message: String) : kotlin.Exception(message)

// Public interface members begin here.


// Interface implemented by anything that can contain an object reference.
//
// Such types expose a `destroy()` method that must be called to cleanly
// dispose of the contained objects. Failure to call this method may result
// in memory leaks.
//
// The easiest way to ensure this method is called is to use the `.use`
// helper method to execute a block and destroy the object at the end.
@OptIn(ExperimentalStdlibApi::class)
public interface Disposable : AutoCloseable {
    public fun destroy()
    override fun close(): Unit = destroy()
    public companion object {
        internal fun destroy(vararg args: Any?) {
            for (arg in args) {
                when (arg) {
                    is Disposable -> arg.destroy()
                    is ArrayList<*> -> {
                        for (idx in arg.indices) {
                            val element = arg[idx]
                            if (element is Disposable) {
                                element.destroy()
                            }
                        }
                    }
                    is Map<*, *> -> {
                        for (element in arg.values) {
                            if (element is Disposable) {
                                element.destroy()
                            }
                        }
                    }
                    is Array<*> -> {
                        for (element in arg) {
                            if (element is Disposable) {
                                element.destroy()
                            }
                        }
                    }
                    is Iterable<*> -> {
                        for (element in arg) {
                            if (element is Disposable) {
                                element.destroy()
                            }
                        }
                    }
                }
            }
        }
    }
}

@OptIn(kotlin.contracts.ExperimentalContracts::class)
public inline fun <T : Disposable?, R> T.use(block: (T) -> R): R {
    kotlin.contracts.contract {
        callsInPlace(block, kotlin.contracts.InvocationKind.EXACTLY_ONCE)
    }
    return try {
        block(this)
    } finally {
        try {
            // N.B. our implementation is on the nullable type `Disposable?`.
            this?.destroy()
        } catch (e: Throwable) {
            // swallow
        }
    }
}

/** Used to instantiate an interface without an actual pointer, for fakes in tests, mostly. */
public object NoPointer



















public interface Bolt11InvoiceInterface {
    
    public fun `amountMilliSatoshis`(): kotlin.ULong?
    
    public fun `currency`(): Currency
    
    public fun `expiryTimeSeconds`(): kotlin.ULong
    
    public fun `fallbackAddresses`(): List<Address>
    
    public fun `invoiceDescription`(): Bolt11InvoiceDescription
    
    public fun `isExpired`(): kotlin.Boolean
    
    public fun `minFinalCltvExpiryDelta`(): kotlin.ULong
    
    public fun `network`(): Network
    
    public fun `paymentHash`(): PaymentHash
    
    public fun `paymentSecret`(): PaymentSecret
    
    public fun `recoverPayeePubKey`(): PublicKey
    
    public fun `routeHints`(): List<List<RouteHintHop>>
    
    public fun `secondsSinceEpoch`(): kotlin.ULong
    
    public fun `secondsUntilExpiry`(): kotlin.ULong
    
    public fun `signableHash`(): List<kotlin.UByte>
    
    public fun `wouldExpire`(`atTimeSeconds`: kotlin.ULong): kotlin.Boolean
    
    public companion object
}




public interface Bolt11PaymentInterface {
    
    @Throws(NodeException::class)
    public fun `claimForHash`(`paymentHash`: PaymentHash, `claimableAmountMsat`: kotlin.ULong, `preimage`: PaymentPreimage)
    
    @Throws(NodeException::class)
    public fun `estimateRoutingFees`(`invoice`: Bolt11Invoice): kotlin.ULong
    
    @Throws(NodeException::class)
    public fun `estimateRoutingFeesUsingAmount`(`invoice`: Bolt11Invoice, `amountMsat`: kotlin.ULong): kotlin.ULong
    
    @Throws(NodeException::class)
    public fun `failForHash`(`paymentHash`: PaymentHash)
    
    @Throws(NodeException::class)
    public fun `receive`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt): Bolt11Invoice
    
    @Throws(NodeException::class)
    public fun `receiveForHash`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `paymentHash`: PaymentHash): Bolt11Invoice
    
    @Throws(NodeException::class)
    public fun `receiveVariableAmount`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt): Bolt11Invoice
    
    @Throws(NodeException::class)
    public fun `receiveVariableAmountForHash`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `paymentHash`: PaymentHash): Bolt11Invoice
    
    @Throws(NodeException::class)
    public fun `receiveVariableAmountViaJitChannel`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxProportionalLspFeeLimitPpmMsat`: kotlin.ULong?): Bolt11Invoice
    
    @Throws(NodeException::class)
    public fun `receiveVariableAmountViaJitChannelForHash`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxProportionalLspFeeLimitPpmMsat`: kotlin.ULong?, `paymentHash`: PaymentHash): Bolt11Invoice
    
    @Throws(NodeException::class)
    public fun `receiveViaJitChannel`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxLspFeeLimitMsat`: kotlin.ULong?): Bolt11Invoice
    
    @Throws(NodeException::class)
    public fun `receiveViaJitChannelForHash`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxLspFeeLimitMsat`: kotlin.ULong?, `paymentHash`: PaymentHash): Bolt11Invoice
    
    @Throws(NodeException::class)
    public fun `send`(`invoice`: Bolt11Invoice, `routeParameters`: RouteParametersConfig?): PaymentId
    
    @Throws(NodeException::class)
    public fun `sendProbes`(`invoice`: Bolt11Invoice, `routeParameters`: RouteParametersConfig?)
    
    @Throws(NodeException::class)
    public fun `sendProbesUsingAmount`(`invoice`: Bolt11Invoice, `amountMsat`: kotlin.ULong, `routeParameters`: RouteParametersConfig?)
    
    @Throws(NodeException::class)
    public fun `sendUsingAmount`(`invoice`: Bolt11Invoice, `amountMsat`: kotlin.ULong, `routeParameters`: RouteParametersConfig?): PaymentId
    
    public companion object
}




public interface Bolt12InvoiceInterface {
    
    public fun `absoluteExpirySeconds`(): kotlin.ULong?
    
    public fun `amount`(): OfferAmount?
    
    public fun `amountMsats`(): kotlin.ULong
    
    public fun `chain`(): List<kotlin.UByte>
    
    public fun `createdAt`(): kotlin.ULong
    
    public fun `encode`(): List<kotlin.UByte>
    
    public fun `fallbackAddresses`(): List<Address>
    
    public fun `invoiceDescription`(): kotlin.String?
    
    public fun `isExpired`(): kotlin.Boolean
    
    public fun `issuer`(): kotlin.String?
    
    public fun `issuerSigningPubkey`(): PublicKey?
    
    public fun `metadata`(): List<kotlin.UByte>?
    
    public fun `offerChains`(): List<List<kotlin.UByte>>?
    
    public fun `payerNote`(): kotlin.String?
    
    public fun `payerSigningPubkey`(): PublicKey
    
    public fun `paymentHash`(): PaymentHash
    
    public fun `quantity`(): kotlin.ULong?
    
    public fun `relativeExpiry`(): kotlin.ULong
    
    public fun `signableHash`(): List<kotlin.UByte>
    
    public fun `signingPubkey`(): PublicKey
    
    public companion object
}




public interface Bolt12PaymentInterface {
    
    @Throws(NodeException::class)
    public fun `blindedPathsForAsyncRecipient`(`recipientId`: kotlin.ByteArray): kotlin.ByteArray
    
    @Throws(NodeException::class)
    public fun `initiateRefund`(`amountMsat`: kotlin.ULong, `expirySecs`: kotlin.UInt, `quantity`: kotlin.ULong?, `payerNote`: kotlin.String?, `routeParameters`: RouteParametersConfig?): Refund
    
    @Throws(NodeException::class)
    public fun `receive`(`amountMsat`: kotlin.ULong, `description`: kotlin.String, `expirySecs`: kotlin.UInt?, `quantity`: kotlin.ULong?): Offer
    
    @Throws(NodeException::class)
    public fun `receiveAsync`(): Offer
    
    @Throws(NodeException::class)
    public fun `receiveVariableAmount`(`description`: kotlin.String, `expirySecs`: kotlin.UInt?): Offer
    
    @Throws(NodeException::class)
    public fun `requestRefundPayment`(`refund`: Refund): Bolt12Invoice
    
    @Throws(NodeException::class)
    public fun `send`(`offer`: Offer, `quantity`: kotlin.ULong?, `payerNote`: kotlin.String?, `routeParameters`: RouteParametersConfig?): PaymentId
    
    @Throws(NodeException::class)
    public fun `sendUsingAmount`(`offer`: Offer, `amountMsat`: kotlin.ULong, `quantity`: kotlin.ULong?, `payerNote`: kotlin.String?, `routeParameters`: RouteParametersConfig?): PaymentId
    
    @Throws(NodeException::class)
    public fun `setPathsToStaticInvoiceServer`(`paths`: kotlin.ByteArray)
    
    public companion object
}




public interface BuilderInterface {
    
    @Throws(BuildException::class)
    public fun `build`(): Node
    
    @Throws(BuildException::class)
    public fun `buildWithFsStore`(): Node
    
    @Throws(BuildException::class)
    public fun `buildWithVssStore`(`vssUrl`: kotlin.String, `storeId`: kotlin.String, `lnurlAuthServerUrl`: kotlin.String, `fixedHeaders`: Map<kotlin.String, kotlin.String>): Node
    
    @Throws(BuildException::class)
    public fun `buildWithVssStoreAndFixedHeaders`(`vssUrl`: kotlin.String, `storeId`: kotlin.String, `fixedHeaders`: Map<kotlin.String, kotlin.String>): Node
    
    @Throws(BuildException::class)
    public fun `buildWithVssStoreAndHeaderProvider`(`vssUrl`: kotlin.String, `storeId`: kotlin.String, `headerProvider`: VssHeaderProvider): Node
    
    public fun `setAddressType`(`addressType`: AddressType)
    
    public fun `setAddressTypesToMonitor`(`addressTypesToMonitor`: List<AddressType>)
    
    @Throws(BuildException::class)
    public fun `setAnnouncementAddresses`(`announcementAddresses`: List<SocketAddress>)
    
    @Throws(BuildException::class)
    public fun `setAsyncPaymentsRole`(`role`: AsyncPaymentsRole?)
    
    public fun `setChainSourceBitcoindRest`(`restHost`: kotlin.String, `restPort`: kotlin.UShort, `rpcHost`: kotlin.String, `rpcPort`: kotlin.UShort, `rpcUser`: kotlin.String, `rpcPassword`: kotlin.String)
    
    public fun `setChainSourceBitcoindRpc`(`rpcHost`: kotlin.String, `rpcPort`: kotlin.UShort, `rpcUser`: kotlin.String, `rpcPassword`: kotlin.String)
    
    public fun `setChainSourceElectrum`(`serverUrl`: kotlin.String, `config`: ElectrumSyncConfig?)
    
    public fun `setChainSourceEsplora`(`serverUrl`: kotlin.String, `config`: EsploraSyncConfig?)
    
    public fun `setChannelDataMigration`(`migration`: ChannelDataMigration)
    
    public fun `setCustomLogger`(`logWriter`: LogWriter)
    
    public fun `setEntropyBip39Mnemonic`(`mnemonic`: Mnemonic, `passphrase`: kotlin.String?)
    
    @Throws(BuildException::class)
    public fun `setEntropySeedBytes`(`seedBytes`: List<kotlin.UByte>)
    
    public fun `setEntropySeedPath`(`seedPath`: kotlin.String)
    
    public fun `setFilesystemLogger`(`logFilePath`: kotlin.String?, `maxLogLevel`: LogLevel?)
    
    public fun `setGossipSourceP2p`()
    
    public fun `setGossipSourceRgs`(`rgsServerUrl`: kotlin.String)
    
    public fun `setLiquiditySourceLsps1`(`nodeId`: PublicKey, `address`: SocketAddress, `token`: kotlin.String?)
    
    public fun `setLiquiditySourceLsps2`(`nodeId`: PublicKey, `address`: SocketAddress, `token`: kotlin.String?)
    
    @Throws(BuildException::class)
    public fun `setListeningAddresses`(`listeningAddresses`: List<SocketAddress>)
    
    public fun `setLogFacadeLogger`()
    
    public fun `setNetwork`(`network`: Network)
    
    @Throws(BuildException::class)
    public fun `setNodeAlias`(`nodeAlias`: kotlin.String)
    
    public fun `setPathfindingScoresSource`(`url`: kotlin.String)
    
    public fun `setStorageDirPath`(`storageDirPath`: kotlin.String)
    
    public companion object
}




public interface FeeRateInterface {
    
    public fun `toSatPerKwu`(): kotlin.ULong
    
    public fun `toSatPerVbCeil`(): kotlin.ULong
    
    public fun `toSatPerVbFloor`(): kotlin.ULong
    
    public companion object
}




public interface Lsps1LiquidityInterface {
    
    @Throws(NodeException::class)
    public fun `checkOrderStatus`(`orderId`: Lsps1OrderId): Lsps1OrderStatus
    
    @Throws(NodeException::class)
    public fun `requestChannel`(`lspBalanceSat`: kotlin.ULong, `clientBalanceSat`: kotlin.ULong, `channelExpiryBlocks`: kotlin.UInt, `announceChannel`: kotlin.Boolean): Lsps1OrderStatus
    
    public companion object
}




public interface LogWriter {
    
    public fun `log`(`record`: LogRecord)
    
    public companion object
}




public interface NetworkGraphInterface {
    
    public fun `channel`(`shortChannelId`: kotlin.ULong): ChannelInfo?
    
    public fun `listChannels`(): List<kotlin.ULong>
    
    public fun `listNodes`(): List<NodeId>
    
    public fun `node`(`nodeId`: NodeId): NodeInfo?
    
    public companion object
}




public interface NodeInterface {
    
    @Throws(NodeException::class)
    public fun `addAddressTypeToMonitor`(`addressType`: AddressType, `seedBytes`: List<kotlin.UByte>)
    
    @Throws(NodeException::class)
    public fun `addAddressTypeToMonitorWithMnemonic`(`addressType`: AddressType, `mnemonic`: Mnemonic, `passphrase`: kotlin.String?)
    
    public fun `announcementAddresses`(): List<SocketAddress>?
    
    public fun `bolt11Payment`(): Bolt11Payment
    
    public fun `bolt12Payment`(): Bolt12Payment
    
    @Throws(NodeException::class)
    public fun `closeChannel`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey)
    
    public fun `config`(): Config
    
    @Throws(NodeException::class)
    public fun `connect`(`nodeId`: PublicKey, `address`: SocketAddress, `persist`: kotlin.Boolean)
    
    public fun `currentSyncIntervals`(): RuntimeSyncIntervals
    
    @Throws(NodeException::class)
    public fun `disconnect`(`nodeId`: PublicKey)
    
    @Throws(NodeException::class)
    public fun `eventHandled`()
    
    @Throws(NodeException::class)
    public fun `exportPathfindingScores`(): kotlin.ByteArray
    
    @Throws(NodeException::class)
    public fun `forceCloseChannel`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `reason`: kotlin.String?)
    
    @Throws(NodeException::class)
    public fun `getAddressBalance`(`addressStr`: kotlin.String): kotlin.ULong
    
    @Throws(NodeException::class)
    public fun `getBalanceForAddressType`(`addressType`: AddressType): AddressTypeBalance
    
    public fun `getTransactionDetails`(`txid`: Txid): TransactionDetails?
    
    public fun `listBalances`(): BalanceDetails
    
    public fun `listChannels`(): List<ChannelDetails>
    
    public fun `listMonitoredAddressTypes`(): List<AddressType>
    
    public fun `listPayments`(): List<PaymentDetails>
    
    public fun `listPeers`(): List<PeerDetails>
    
    public fun `listeningAddresses`(): List<SocketAddress>?
    
    public fun `lsps1Liquidity`(): Lsps1Liquidity
    
    public fun `networkGraph`(): NetworkGraph
    
    public fun `nextEvent`(): Event?
    
    public suspend fun `nextEventAsync`(): Event
    
    public fun `nodeAlias`(): NodeAlias?
    
    public fun `nodeId`(): PublicKey
    
    public fun `onchainPayment`(): OnchainPayment
    
    @Throws(NodeException::class)
    public fun `openAnnouncedChannel`(`nodeId`: PublicKey, `address`: SocketAddress, `channelAmountSats`: kotlin.ULong, `pushToCounterpartyMsat`: kotlin.ULong?, `channelConfig`: ChannelConfig?): UserChannelId
    
    @Throws(NodeException::class)
    public fun `openChannel`(`nodeId`: PublicKey, `address`: SocketAddress, `channelAmountSats`: kotlin.ULong, `pushToCounterpartyMsat`: kotlin.ULong?, `channelConfig`: ChannelConfig?): UserChannelId
    
    public fun `payment`(`paymentId`: PaymentId): PaymentDetails?
    
    @Throws(NodeException::class)
    public fun `removeAddressTypeFromMonitor`(`addressType`: AddressType)
    
    @Throws(NodeException::class)
    public fun `removePayment`(`paymentId`: PaymentId)
    
    @Throws(NodeException::class)
    public fun `setPrimaryAddressType`(`addressType`: AddressType, `seedBytes`: List<kotlin.UByte>)
    
    @Throws(NodeException::class)
    public fun `setPrimaryAddressTypeWithMnemonic`(`addressType`: AddressType, `mnemonic`: Mnemonic, `passphrase`: kotlin.String?)
    
    public fun `signMessage`(`msg`: List<kotlin.UByte>): kotlin.String
    
    @Throws(NodeException::class)
    public fun `spliceIn`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `spliceAmountSats`: kotlin.ULong)
    
    @Throws(NodeException::class)
    public fun `spliceOut`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `address`: Address, `spliceAmountSats`: kotlin.ULong)
    
    public fun `spontaneousPayment`(): SpontaneousPayment
    
    @Throws(NodeException::class)
    public fun `start`()
    
    public fun `status`(): NodeStatus
    
    @Throws(NodeException::class)
    public fun `stop`()
    
    @Throws(NodeException::class)
    public fun `syncWallets`()
    
    public fun `unifiedQrPayment`(): UnifiedQrPayment
    
    @Throws(NodeException::class)
    public fun `updateChannelConfig`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `channelConfig`: ChannelConfig)
    
    @Throws(NodeException::class)
    public fun `updateSyncIntervals`(`intervals`: RuntimeSyncIntervals)
    
    public fun `verifySignature`(`msg`: List<kotlin.UByte>, `sig`: kotlin.String, `pkey`: PublicKey): kotlin.Boolean
    
    public fun `waitNextEvent`(): Event
    
    public companion object
}




public interface OfferInterface {
    
    public fun `absoluteExpirySeconds`(): kotlin.ULong?
    
    public fun `amount`(): OfferAmount?
    
    public fun `chains`(): List<Network>
    
    public fun `expectsQuantity`(): kotlin.Boolean
    
    public fun `id`(): OfferId
    
    public fun `isExpired`(): kotlin.Boolean
    
    public fun `isValidQuantity`(`quantity`: kotlin.ULong): kotlin.Boolean
    
    public fun `issuer`(): kotlin.String?
    
    public fun `issuerSigningPubkey`(): PublicKey?
    
    public fun `metadata`(): List<kotlin.UByte>?
    
    public fun `offerDescription`(): kotlin.String?
    
    public fun `supportsChain`(`chain`: Network): kotlin.Boolean
    
    public companion object
}




public interface OnchainPaymentInterface {
    
    @Throws(NodeException::class)
    public fun `accelerateByCpfp`(`txid`: Txid, `feeRate`: FeeRate?, `destinationAddress`: Address?): Txid
    
    @Throws(NodeException::class)
    public fun `bumpFeeByRbf`(`txid`: Txid, `feeRate`: FeeRate): Txid
    
    @Throws(NodeException::class)
    public fun `calculateCpfpFeeRate`(`parentTxid`: Txid, `urgent`: kotlin.Boolean): FeeRate
    
    @Throws(NodeException::class)
    public fun `calculateSendAllFee`(`address`: Address, `retainReserves`: kotlin.Boolean, `feeRate`: FeeRate?): kotlin.ULong
    
    @Throws(NodeException::class)
    public fun `calculateTotalFee`(`address`: Address, `amountSats`: kotlin.ULong, `feeRate`: FeeRate?, `utxosToSpend`: List<SpendableUtxo>?): kotlin.ULong
    
    @Throws(NodeException::class)
    public fun `listSpendableOutputs`(): List<SpendableUtxo>
    
    @Throws(NodeException::class)
    public fun `newAddress`(): Address
    
    @Throws(NodeException::class)
    public fun `newAddressForType`(`addressType`: AddressType): Address
    
    @Throws(NodeException::class)
    public fun `selectUtxosWithAlgorithm`(`targetAmountSats`: kotlin.ULong, `feeRate`: FeeRate?, `algorithm`: CoinSelectionAlgorithm, `utxos`: List<SpendableUtxo>?): List<SpendableUtxo>
    
    @Throws(NodeException::class)
    public fun `sendAllToAddress`(`address`: Address, `retainReserve`: kotlin.Boolean, `feeRate`: FeeRate?): Txid
    
    @Throws(NodeException::class)
    public fun `sendToAddress`(`address`: Address, `amountSats`: kotlin.ULong, `feeRate`: FeeRate?, `utxosToSpend`: List<SpendableUtxo>?): Txid
    
    public companion object
}




public interface RefundInterface {
    
    public fun `absoluteExpirySeconds`(): kotlin.ULong?
    
    public fun `amountMsats`(): kotlin.ULong
    
    public fun `chain`(): Network?
    
    public fun `isExpired`(): kotlin.Boolean
    
    public fun `issuer`(): kotlin.String?
    
    public fun `payerMetadata`(): List<kotlin.UByte>
    
    public fun `payerNote`(): kotlin.String?
    
    public fun `payerSigningPubkey`(): PublicKey
    
    public fun `quantity`(): kotlin.ULong?
    
    public fun `refundDescription`(): kotlin.String
    
    public companion object
}




public interface SpontaneousPaymentInterface {
    
    @Throws(NodeException::class)
    public fun `send`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `routeParameters`: RouteParametersConfig?): PaymentId
    
    @Throws(NodeException::class)
    public fun `sendProbes`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey)
    
    @Throws(NodeException::class)
    public fun `sendWithCustomTlvs`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `routeParameters`: RouteParametersConfig?, `customTlvs`: List<CustomTlvRecord>): PaymentId
    
    @Throws(NodeException::class)
    public fun `sendWithPreimage`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `preimage`: PaymentPreimage, `routeParameters`: RouteParametersConfig?): PaymentId
    
    @Throws(NodeException::class)
    public fun `sendWithPreimageAndCustomTlvs`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `customTlvs`: List<CustomTlvRecord>, `preimage`: PaymentPreimage, `routeParameters`: RouteParametersConfig?): PaymentId
    
    public companion object
}




public interface UnifiedQrPaymentInterface {
    
    @Throws(NodeException::class)
    public fun `receive`(`amountSats`: kotlin.ULong, `message`: kotlin.String, `expirySec`: kotlin.UInt): kotlin.String
    
    @Throws(NodeException::class)
    public fun `send`(`uriStr`: kotlin.String, `routeParameters`: RouteParametersConfig?): QrPaymentResult
    
    public companion object
}




public interface VssHeaderProviderInterface {
    
    @Throws(VssHeaderProviderException::class, kotlin.coroutines.cancellation.CancellationException::class)
    public suspend fun `getHeaders`(`request`: List<kotlin.UByte>): Map<kotlin.String, kotlin.String>
    
    public companion object
}




@kotlinx.serialization.Serializable
public data class AddressTypeBalance (
    val `totalSats`: kotlin.ULong, 
    val `spendableSats`: kotlin.ULong
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class AnchorChannelsConfig (
    val `trustedPeersNoReserve`: List<PublicKey>, 
    val `perChannelReserveSats`: kotlin.ULong
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class BackgroundSyncConfig (
    val `onchainWalletSyncIntervalSecs`: kotlin.ULong, 
    val `lightningWalletSyncIntervalSecs`: kotlin.ULong, 
    val `feeRateCacheUpdateIntervalSecs`: kotlin.ULong
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class BalanceDetails (
    val `totalOnchainBalanceSats`: kotlin.ULong, 
    val `spendableOnchainBalanceSats`: kotlin.ULong, 
    val `totalAnchorChannelsReserveSats`: kotlin.ULong, 
    val `totalLightningBalanceSats`: kotlin.ULong, 
    val `lightningBalances`: List<LightningBalance>, 
    val `pendingBalancesFromChannelClosures`: List<PendingSweepBalance>
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class BestBlock (
    val `blockHash`: BlockHash, 
    val `height`: kotlin.UInt
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class ChannelConfig (
    val `forwardingFeeProportionalMillionths`: kotlin.UInt, 
    val `forwardingFeeBaseMsat`: kotlin.UInt, 
    val `cltvExpiryDelta`: kotlin.UShort, 
    val `maxDustHtlcExposure`: MaxDustHtlcExposure, 
    val `forceCloseAvoidanceMaxFeeSatoshis`: kotlin.ULong, 
    val `acceptUnderpayingHtlcs`: kotlin.Boolean
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class ChannelDataMigration (
    val `channelManager`: List<kotlin.UByte>?, 
    val `channelMonitors`: List<List<kotlin.UByte>>
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class ChannelDetails (
    val `channelId`: ChannelId, 
    val `counterpartyNodeId`: PublicKey, 
    val `fundingTxo`: OutPoint?, 
    val `shortChannelId`: kotlin.ULong?, 
    val `outboundScidAlias`: kotlin.ULong?, 
    val `inboundScidAlias`: kotlin.ULong?, 
    val `channelValueSats`: kotlin.ULong, 
    val `unspendablePunishmentReserve`: kotlin.ULong?, 
    val `userChannelId`: UserChannelId, 
    val `feerateSatPer1000Weight`: kotlin.UInt, 
    val `outboundCapacityMsat`: kotlin.ULong, 
    val `inboundCapacityMsat`: kotlin.ULong, 
    val `confirmationsRequired`: kotlin.UInt?, 
    val `confirmations`: kotlin.UInt?, 
    val `isOutbound`: kotlin.Boolean, 
    val `isChannelReady`: kotlin.Boolean, 
    val `isUsable`: kotlin.Boolean, 
    val `isAnnounced`: kotlin.Boolean, 
    val `cltvExpiryDelta`: kotlin.UShort?, 
    val `counterpartyUnspendablePunishmentReserve`: kotlin.ULong, 
    val `counterpartyOutboundHtlcMinimumMsat`: kotlin.ULong?, 
    val `counterpartyOutboundHtlcMaximumMsat`: kotlin.ULong?, 
    val `counterpartyForwardingInfoFeeBaseMsat`: kotlin.UInt?, 
    val `counterpartyForwardingInfoFeeProportionalMillionths`: kotlin.UInt?, 
    val `counterpartyForwardingInfoCltvExpiryDelta`: kotlin.UShort?, 
    val `nextOutboundHtlcLimitMsat`: kotlin.ULong, 
    val `nextOutboundHtlcMinimumMsat`: kotlin.ULong, 
    val `forceCloseSpendDelay`: kotlin.UShort?, 
    val `inboundHtlcMinimumMsat`: kotlin.ULong, 
    val `inboundHtlcMaximumMsat`: kotlin.ULong?, 
    val `config`: ChannelConfig, 
    val `claimableOnCloseSats`: kotlin.ULong?
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class ChannelInfo (
    val `nodeOne`: NodeId, 
    val `oneToTwo`: ChannelUpdateInfo?, 
    val `nodeTwo`: NodeId, 
    val `twoToOne`: ChannelUpdateInfo?, 
    val `capacitySats`: kotlin.ULong?
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class ChannelUpdateInfo (
    val `lastUpdate`: kotlin.UInt, 
    val `enabled`: kotlin.Boolean, 
    val `cltvExpiryDelta`: kotlin.UShort, 
    val `htlcMinimumMsat`: kotlin.ULong, 
    val `htlcMaximumMsat`: kotlin.ULong, 
    val `fees`: RoutingFees
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class Config (
    val `storageDirPath`: kotlin.String, 
    val `network`: Network, 
    val `listeningAddresses`: List<SocketAddress>?, 
    val `announcementAddresses`: List<SocketAddress>?, 
    val `nodeAlias`: NodeAlias?, 
    val `trustedPeers0conf`: List<PublicKey>, 
    val `probingLiquidityLimitMultiplier`: kotlin.ULong, 
    val `anchorChannelsConfig`: AnchorChannelsConfig?, 
    val `routeParameters`: RouteParametersConfig?, 
    val `includeUntrustedPendingInSpendable`: kotlin.Boolean, 
    val `addressType`: AddressType, 
    val `addressTypesToMonitor`: List<AddressType>
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class CustomTlvRecord (
    val `typeNum`: kotlin.ULong, 
    val `value`: List<kotlin.UByte>
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class ElectrumSyncConfig (
    val `backgroundSyncConfig`: BackgroundSyncConfig?
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class EsploraSyncConfig (
    val `backgroundSyncConfig`: BackgroundSyncConfig?
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class LspFeeLimits (
    val `maxTotalOpeningFeeMsat`: kotlin.ULong?, 
    val `maxProportionalOpeningFeePpmMsat`: kotlin.ULong?
) {
    public companion object
}




public data class Lsps1Bolt11PaymentInfo (
    val `state`: Lsps1PaymentState, 
    val `expiresAt`: LspsDateTime, 
    val `feeTotalSat`: kotlin.ULong, 
    val `orderTotalSat`: kotlin.ULong, 
    val `invoice`: Bolt11Invoice
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`state`,
            this.`expiresAt`,
            this.`feeTotalSat`,
            this.`orderTotalSat`,
            this.`invoice`,
        )
    }
    public companion object
}



@kotlinx.serialization.Serializable
public data class Lsps1ChannelInfo (
    val `fundedAt`: LspsDateTime, 
    val `fundingOutpoint`: OutPoint, 
    val `expiresAt`: LspsDateTime
) {
    public companion object
}




public data class Lsps1OnchainPaymentInfo (
    val `state`: Lsps1PaymentState, 
    val `expiresAt`: LspsDateTime, 
    val `feeTotalSat`: kotlin.ULong, 
    val `orderTotalSat`: kotlin.ULong, 
    val `address`: Address, 
    val `minOnchainPaymentConfirmations`: kotlin.UShort?, 
    val `minFeeFor0conf`: FeeRate, 
    val `refundOnchainAddress`: Address?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`state`,
            this.`expiresAt`,
            this.`feeTotalSat`,
            this.`orderTotalSat`,
            this.`address`,
            this.`minOnchainPaymentConfirmations`,
            this.`minFeeFor0conf`,
            this.`refundOnchainAddress`,
        )
    }
    public companion object
}



@kotlinx.serialization.Serializable
public data class Lsps1OrderParams (
    val `lspBalanceSat`: kotlin.ULong, 
    val `clientBalanceSat`: kotlin.ULong, 
    val `requiredChannelConfirmations`: kotlin.UShort, 
    val `fundingConfirmsWithinBlocks`: kotlin.UShort, 
    val `channelExpiryBlocks`: kotlin.UInt, 
    val `token`: kotlin.String?, 
    val `announceChannel`: kotlin.Boolean
) {
    public companion object
}




public data class Lsps1OrderStatus (
    val `orderId`: Lsps1OrderId, 
    val `orderParams`: Lsps1OrderParams, 
    val `paymentOptions`: Lsps1PaymentInfo, 
    val `channelState`: Lsps1ChannelInfo?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`orderId`,
            this.`orderParams`,
            this.`paymentOptions`,
            this.`channelState`,
        )
    }
    public companion object
}




public data class Lsps1PaymentInfo (
    val `bolt11`: Lsps1Bolt11PaymentInfo?, 
    val `onchain`: Lsps1OnchainPaymentInfo?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`bolt11`,
            this.`onchain`,
        )
    }
    public companion object
}



@kotlinx.serialization.Serializable
public data class Lsps2ServiceConfig (
    val `requireToken`: kotlin.String?, 
    val `advertiseService`: kotlin.Boolean, 
    val `channelOpeningFeePpm`: kotlin.UInt, 
    val `channelOverProvisioningPpm`: kotlin.UInt, 
    val `minChannelOpeningFeeMsat`: kotlin.ULong, 
    val `minChannelLifetime`: kotlin.UInt, 
    val `maxClientToSelfDelay`: kotlin.UInt, 
    val `minPaymentSizeMsat`: kotlin.ULong, 
    val `maxPaymentSizeMsat`: kotlin.ULong, 
    val `clientTrustsLsp`: kotlin.Boolean
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class LogRecord (
    val `level`: LogLevel, 
    val `args`: kotlin.String, 
    val `modulePath`: kotlin.String, 
    val `line`: kotlin.UInt
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class NodeAnnouncementInfo (
    val `lastUpdate`: kotlin.UInt, 
    val `alias`: kotlin.String, 
    val `addresses`: List<SocketAddress>
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class NodeInfo (
    val `channels`: List<kotlin.ULong>, 
    val `announcementInfo`: NodeAnnouncementInfo?
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class NodeStatus (
    val `isRunning`: kotlin.Boolean, 
    val `currentBestBlock`: BestBlock, 
    val `latestLightningWalletSyncTimestamp`: kotlin.ULong?, 
    val `latestOnchainWalletSyncTimestamp`: kotlin.ULong?, 
    val `latestFeeRateCacheUpdateTimestamp`: kotlin.ULong?, 
    val `latestRgsSnapshotTimestamp`: kotlin.ULong?, 
    val `latestPathfindingScoresSyncTimestamp`: kotlin.ULong?, 
    val `latestNodeAnnouncementBroadcastTimestamp`: kotlin.ULong?, 
    val `latestChannelMonitorArchivalHeight`: kotlin.UInt?
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class OutPoint (
    val `txid`: Txid, 
    val `vout`: kotlin.UInt
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class PaymentDetails (
    val `id`: PaymentId, 
    val `kind`: PaymentKind, 
    val `amountMsat`: kotlin.ULong?, 
    val `feePaidMsat`: kotlin.ULong?, 
    val `direction`: PaymentDirection, 
    val `status`: PaymentStatus, 
    val `latestUpdateTimestamp`: kotlin.ULong
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class PeerDetails (
    val `nodeId`: PublicKey, 
    val `address`: SocketAddress, 
    val `isPersisted`: kotlin.Boolean, 
    val `isConnected`: kotlin.Boolean
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class RouteHintHop (
    val `srcNodeId`: PublicKey, 
    val `shortChannelId`: kotlin.ULong, 
    val `cltvExpiryDelta`: kotlin.UShort, 
    val `htlcMinimumMsat`: kotlin.ULong?, 
    val `htlcMaximumMsat`: kotlin.ULong?, 
    val `fees`: RoutingFees
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class RouteParametersConfig (
    val `maxTotalRoutingFeeMsat`: kotlin.ULong?, 
    val `maxTotalCltvExpiryDelta`: kotlin.UInt, 
    val `maxPathCount`: kotlin.UByte, 
    val `maxChannelSaturationPowerOfHalf`: kotlin.UByte
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class RoutingFees (
    val `baseMsat`: kotlin.UInt, 
    val `proportionalMillionths`: kotlin.UInt
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class RuntimeSyncIntervals (
    val `onchainWalletSyncIntervalSecs`: kotlin.ULong, 
    val `lightningWalletSyncIntervalSecs`: kotlin.ULong, 
    val `feeRateCacheUpdateIntervalSecs`: kotlin.ULong
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class SpendableUtxo (
    val `outpoint`: OutPoint, 
    val `valueSats`: kotlin.ULong
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class TransactionDetails (
    val `amountSats`: kotlin.Long, 
    val `inputs`: List<TxInput>, 
    val `outputs`: List<TxOutput>
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class TxInput (
    val `txid`: Txid, 
    val `vout`: kotlin.UInt, 
    val `scriptsig`: kotlin.String, 
    val `witness`: List<kotlin.String>, 
    val `sequence`: kotlin.UInt
) {
    public companion object
}



@kotlinx.serialization.Serializable
public data class TxOutput (
    val `scriptpubkey`: kotlin.String, 
    val `scriptpubkeyType`: kotlin.String?, 
    val `scriptpubkeyAddress`: kotlin.String?, 
    val `value`: kotlin.Long, 
    val `n`: kotlin.UInt
) {
    public companion object
}





@kotlinx.serialization.Serializable
public enum class AddressType {
    
    LEGACY,
    NESTED_SEGWIT,
    NATIVE_SEGWIT,
    TAPROOT;
    public companion object
}







@kotlinx.serialization.Serializable
public enum class AsyncPaymentsRole {
    
    CLIENT,
    SERVER;
    public companion object
}







@kotlinx.serialization.Serializable
public enum class BalanceSource {
    
    HOLDER_FORCE_CLOSED,
    COUNTERPARTY_FORCE_CLOSED,
    COOP_CLOSE,
    HTLC;
    public companion object
}






@kotlinx.serialization.Serializable
public sealed class Bolt11InvoiceDescription {
    @kotlinx.serialization.Serializable
    public data class Hash(
        val `hash`: kotlin.String,
    ) : Bolt11InvoiceDescription() {
    }
    @kotlinx.serialization.Serializable
    public data class Direct(
        val `description`: kotlin.String,
    ) : Bolt11InvoiceDescription() {
    }
    
}







public sealed class BuildException(message: String): kotlin.Exception(message) {
    
    public class InvalidSeedBytes(message: String) : BuildException(message)
    
    public class InvalidSeedFile(message: String) : BuildException(message)
    
    public class InvalidSystemTime(message: String) : BuildException(message)
    
    public class InvalidChannelMonitor(message: String) : BuildException(message)
    
    public class InvalidListeningAddresses(message: String) : BuildException(message)
    
    public class InvalidAnnouncementAddresses(message: String) : BuildException(message)
    
    public class InvalidNodeAlias(message: String) : BuildException(message)
    
    public class RuntimeSetupFailed(message: String) : BuildException(message)
    
    public class ReadFailed(message: String) : BuildException(message)
    
    public class WriteFailed(message: String) : BuildException(message)
    
    public class StoragePathAccessFailed(message: String) : BuildException(message)
    
    public class KvStoreSetupFailed(message: String) : BuildException(message)
    
    public class WalletSetupFailed(message: String) : BuildException(message)
    
    public class LoggerSetupFailed(message: String) : BuildException(message)
    
    public class NetworkMismatch(message: String) : BuildException(message)
    
    public class AsyncPaymentsConfigMismatch(message: String) : BuildException(message)
    
}




@kotlinx.serialization.Serializable
public sealed class ClosureReason {
    @kotlinx.serialization.Serializable
    public data class CounterpartyForceClosed(
        val `peerMsg`: UntrustedString,
    ) : ClosureReason() {
    }
    @kotlinx.serialization.Serializable
    public data class HolderForceClosed(
        val `broadcastedLatestTxn`: kotlin.Boolean?,
        val `message`: kotlin.String,
    ) : ClosureReason() {
    }
    
    @kotlinx.serialization.Serializable
    public data object LegacyCooperativeClosure : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    public data object CounterpartyInitiatedCooperativeClosure : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    public data object LocallyInitiatedCooperativeClosure : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    public data object CommitmentTxConfirmed : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    public data object FundingTimedOut : ClosureReason() 
    
    @kotlinx.serialization.Serializable
    public data class ProcessingError(
        val `err`: kotlin.String,
    ) : ClosureReason() {
    }
    
    @kotlinx.serialization.Serializable
    public data object DisconnectedPeer : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    public data object OutdatedChannelManager : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    public data object CounterpartyCoopClosedUnfundedChannel : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    public data object LocallyCoopClosedUnfundedChannel : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    public data object FundingBatchClosure : ClosureReason() 
    
    @kotlinx.serialization.Serializable
    public data class HtlCsTimedOut(
        val `paymentHash`: PaymentHash?,
    ) : ClosureReason() {
    }
    @kotlinx.serialization.Serializable
    public data class PeerFeerateTooLow(
        val `peerFeerateSatPerKw`: kotlin.UInt,
        val `requiredFeerateSatPerKw`: kotlin.UInt,
    ) : ClosureReason() {
    }
    
}







@kotlinx.serialization.Serializable
public enum class CoinSelectionAlgorithm {
    
    BRANCH_AND_BOUND,
    LARGEST_FIRST,
    OLDEST_FIRST,
    SINGLE_RANDOM_DRAW;
    public companion object
}






@kotlinx.serialization.Serializable
public sealed class ConfirmationStatus {
    @kotlinx.serialization.Serializable
    public data class Confirmed(
        val `blockHash`: BlockHash,
        val `height`: kotlin.UInt,
        val `timestamp`: kotlin.ULong,
    ) : ConfirmationStatus() {
    }
    
    @kotlinx.serialization.Serializable
    public data object Unconfirmed : ConfirmationStatus() 
    
    
}







@kotlinx.serialization.Serializable
public enum class Currency {
    
    BITCOIN,
    BITCOIN_TESTNET,
    REGTEST,
    SIMNET,
    SIGNET;
    public companion object
}






@kotlinx.serialization.Serializable
public sealed class Event {
    @kotlinx.serialization.Serializable
    public data class PaymentSuccessful(
        val `paymentId`: PaymentId?,
        val `paymentHash`: PaymentHash,
        val `paymentPreimage`: PaymentPreimage?,
        val `feePaidMsat`: kotlin.ULong?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class PaymentFailed(
        val `paymentId`: PaymentId?,
        val `paymentHash`: PaymentHash?,
        val `reason`: PaymentFailureReason?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class PaymentReceived(
        val `paymentId`: PaymentId?,
        val `paymentHash`: PaymentHash,
        val `amountMsat`: kotlin.ULong,
        val `customRecords`: List<CustomTlvRecord>,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class PaymentClaimable(
        val `paymentId`: PaymentId,
        val `paymentHash`: PaymentHash,
        val `claimableAmountMsat`: kotlin.ULong,
        val `claimDeadline`: kotlin.UInt?,
        val `customRecords`: List<CustomTlvRecord>,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class PaymentForwarded(
        val `prevChannelId`: ChannelId,
        val `nextChannelId`: ChannelId,
        val `prevUserChannelId`: UserChannelId?,
        val `nextUserChannelId`: UserChannelId?,
        val `prevNodeId`: PublicKey?,
        val `nextNodeId`: PublicKey?,
        val `totalFeeEarnedMsat`: kotlin.ULong?,
        val `skimmedFeeMsat`: kotlin.ULong?,
        val `claimFromOnchainTx`: kotlin.Boolean,
        val `outboundAmountForwardedMsat`: kotlin.ULong?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class ChannelPending(
        val `channelId`: ChannelId,
        val `userChannelId`: UserChannelId,
        val `formerTemporaryChannelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `fundingTxo`: OutPoint,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class ChannelReady(
        val `channelId`: ChannelId,
        val `userChannelId`: UserChannelId,
        val `counterpartyNodeId`: PublicKey?,
        val `fundingTxo`: OutPoint?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class ChannelClosed(
        val `channelId`: ChannelId,
        val `userChannelId`: UserChannelId,
        val `counterpartyNodeId`: PublicKey?,
        val `reason`: ClosureReason?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class SplicePending(
        val `channelId`: ChannelId,
        val `userChannelId`: UserChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `newFundingTxo`: OutPoint,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class SpliceFailed(
        val `channelId`: ChannelId,
        val `userChannelId`: UserChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `abandonedFundingTxo`: OutPoint?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class OnchainTransactionConfirmed(
        val `txid`: Txid,
        val `blockHash`: BlockHash,
        val `blockHeight`: kotlin.UInt,
        val `confirmationTime`: kotlin.ULong,
        val `details`: TransactionDetails,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class OnchainTransactionReceived(
        val `txid`: Txid,
        val `details`: TransactionDetails,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class OnchainTransactionReplaced(
        val `txid`: Txid,
        val `conflicts`: List<Txid>,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class OnchainTransactionReorged(
        val `txid`: Txid,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class OnchainTransactionEvicted(
        val `txid`: Txid,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class SyncProgress(
        val `syncType`: SyncType,
        val `progressPercent`: kotlin.UByte,
        val `currentBlockHeight`: kotlin.UInt,
        val `targetBlockHeight`: kotlin.UInt,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class SyncCompleted(
        val `syncType`: SyncType,
        val `syncedBlockHeight`: kotlin.UInt,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    public data class BalanceChanged(
        val `oldSpendableOnchainBalanceSats`: kotlin.ULong,
        val `newSpendableOnchainBalanceSats`: kotlin.ULong,
        val `oldTotalOnchainBalanceSats`: kotlin.ULong,
        val `newTotalOnchainBalanceSats`: kotlin.ULong,
        val `oldTotalLightningBalanceSats`: kotlin.ULong,
        val `newTotalLightningBalanceSats`: kotlin.ULong,
    ) : Event() {
    }
    
}







@kotlinx.serialization.Serializable
public enum class Lsps1PaymentState {
    
    EXPECT_PAYMENT,
    PAID,
    REFUNDED;
    public companion object
}






@kotlinx.serialization.Serializable
public sealed class LightningBalance {
    @kotlinx.serialization.Serializable
    public data class ClaimableOnChannelClose(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
        val `transactionFeeSatoshis`: kotlin.ULong,
        val `outboundPaymentHtlcRoundedMsat`: kotlin.ULong,
        val `outboundForwardedHtlcRoundedMsat`: kotlin.ULong,
        val `inboundClaimingHtlcRoundedMsat`: kotlin.ULong,
        val `inboundHtlcRoundedMsat`: kotlin.ULong,
    ) : LightningBalance() {
    }
    @kotlinx.serialization.Serializable
    public data class ClaimableAwaitingConfirmations(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
        val `confirmationHeight`: kotlin.UInt,
        val `source`: BalanceSource,
    ) : LightningBalance() {
    }
    @kotlinx.serialization.Serializable
    public data class ContentiousClaimable(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
        val `timeoutHeight`: kotlin.UInt,
        val `paymentHash`: PaymentHash,
        val `paymentPreimage`: PaymentPreimage,
    ) : LightningBalance() {
    }
    @kotlinx.serialization.Serializable
    public data class MaybeTimeoutClaimableHtlc(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
        val `claimableHeight`: kotlin.UInt,
        val `paymentHash`: PaymentHash,
        val `outboundPayment`: kotlin.Boolean,
    ) : LightningBalance() {
    }
    @kotlinx.serialization.Serializable
    public data class MaybePreimageClaimableHtlc(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
        val `expiryHeight`: kotlin.UInt,
        val `paymentHash`: PaymentHash,
    ) : LightningBalance() {
    }
    @kotlinx.serialization.Serializable
    public data class CounterpartyRevokedOutputClaimable(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
    ) : LightningBalance() {
    }
    
}







@kotlinx.serialization.Serializable
public enum class LogLevel {
    
    GOSSIP,
    TRACE,
    DEBUG,
    INFO,
    WARN,
    ERROR;
    public companion object
}






@kotlinx.serialization.Serializable
public sealed class MaxDustHtlcExposure {
    @kotlinx.serialization.Serializable
    public data class FixedLimit(
        val `limitMsat`: kotlin.ULong,
    ) : MaxDustHtlcExposure() {
    }
    @kotlinx.serialization.Serializable
    public data class FeeRateMultiplier(
        val `multiplier`: kotlin.ULong,
    ) : MaxDustHtlcExposure() {
    }
    
}







@kotlinx.serialization.Serializable
public enum class Network {
    
    BITCOIN,
    TESTNET,
    SIGNET,
    REGTEST;
    public companion object
}







public sealed class NodeException(message: String): kotlin.Exception(message) {
    
    public class AlreadyRunning(message: String) : NodeException(message)
    
    public class NotRunning(message: String) : NodeException(message)
    
    public class OnchainTxCreationFailed(message: String) : NodeException(message)
    
    public class ConnectionFailed(message: String) : NodeException(message)
    
    public class InvoiceCreationFailed(message: String) : NodeException(message)
    
    public class InvoiceRequestCreationFailed(message: String) : NodeException(message)
    
    public class OfferCreationFailed(message: String) : NodeException(message)
    
    public class RefundCreationFailed(message: String) : NodeException(message)
    
    public class PaymentSendingFailed(message: String) : NodeException(message)
    
    public class InvalidCustomTlvs(message: String) : NodeException(message)
    
    public class ProbeSendingFailed(message: String) : NodeException(message)
    
    public class RouteNotFound(message: String) : NodeException(message)
    
    public class ChannelCreationFailed(message: String) : NodeException(message)
    
    public class ChannelClosingFailed(message: String) : NodeException(message)
    
    public class ChannelSplicingFailed(message: String) : NodeException(message)
    
    public class ChannelConfigUpdateFailed(message: String) : NodeException(message)
    
    public class PersistenceFailed(message: String) : NodeException(message)
    
    public class FeerateEstimationUpdateFailed(message: String) : NodeException(message)
    
    public class FeerateEstimationUpdateTimeout(message: String) : NodeException(message)
    
    public class WalletOperationFailed(message: String) : NodeException(message)
    
    public class WalletOperationTimeout(message: String) : NodeException(message)
    
    public class OnchainTxSigningFailed(message: String) : NodeException(message)
    
    public class TxSyncFailed(message: String) : NodeException(message)
    
    public class TxSyncTimeout(message: String) : NodeException(message)
    
    public class GossipUpdateFailed(message: String) : NodeException(message)
    
    public class GossipUpdateTimeout(message: String) : NodeException(message)
    
    public class LiquidityRequestFailed(message: String) : NodeException(message)
    
    public class UriParameterParsingFailed(message: String) : NodeException(message)
    
    public class InvalidAddress(message: String) : NodeException(message)
    
    public class InvalidSocketAddress(message: String) : NodeException(message)
    
    public class InvalidPublicKey(message: String) : NodeException(message)
    
    public class InvalidSecretKey(message: String) : NodeException(message)
    
    public class InvalidOfferId(message: String) : NodeException(message)
    
    public class InvalidNodeId(message: String) : NodeException(message)
    
    public class InvalidPaymentId(message: String) : NodeException(message)
    
    public class InvalidPaymentHash(message: String) : NodeException(message)
    
    public class InvalidPaymentPreimage(message: String) : NodeException(message)
    
    public class InvalidPaymentSecret(message: String) : NodeException(message)
    
    public class InvalidAmount(message: String) : NodeException(message)
    
    public class InvalidInvoice(message: String) : NodeException(message)
    
    public class InvalidOffer(message: String) : NodeException(message)
    
    public class InvalidRefund(message: String) : NodeException(message)
    
    public class InvalidChannelId(message: String) : NodeException(message)
    
    public class InvalidNetwork(message: String) : NodeException(message)
    
    public class InvalidUri(message: String) : NodeException(message)
    
    public class InvalidQuantity(message: String) : NodeException(message)
    
    public class InvalidNodeAlias(message: String) : NodeException(message)
    
    public class InvalidDateTime(message: String) : NodeException(message)
    
    public class InvalidFeeRate(message: String) : NodeException(message)
    
    public class DuplicatePayment(message: String) : NodeException(message)
    
    public class UnsupportedCurrency(message: String) : NodeException(message)
    
    public class InsufficientFunds(message: String) : NodeException(message)
    
    public class LiquiditySourceUnavailable(message: String) : NodeException(message)
    
    public class LiquidityFeeTooHigh(message: String) : NodeException(message)
    
    public class InvalidBlindedPaths(message: String) : NodeException(message)
    
    public class AsyncPaymentServicesDisabled(message: String) : NodeException(message)
    
    public class CannotRbfFundingTransaction(message: String) : NodeException(message)
    
    public class TransactionNotFound(message: String) : NodeException(message)
    
    public class TransactionAlreadyConfirmed(message: String) : NodeException(message)
    
    public class NoSpendableOutputs(message: String) : NodeException(message)
    
    public class CoinSelectionFailed(message: String) : NodeException(message)
    
    public class InvalidMnemonic(message: String) : NodeException(message)
    
    public class BackgroundSyncNotEnabled(message: String) : NodeException(message)
    
    public class AddressTypeAlreadyMonitored(message: String) : NodeException(message)
    
    public class AddressTypeIsPrimary(message: String) : NodeException(message)
    
    public class AddressTypeNotMonitored(message: String) : NodeException(message)
    
    public class InvalidSeedBytes(message: String) : NodeException(message)
    
}




@kotlinx.serialization.Serializable
public sealed class OfferAmount {
    @kotlinx.serialization.Serializable
    public data class Bitcoin(
        val `amountMsats`: kotlin.ULong,
    ) : OfferAmount() {
    }
    @kotlinx.serialization.Serializable
    public data class Currency(
        val `iso4217Code`: kotlin.String,
        val `amount`: kotlin.ULong,
    ) : OfferAmount() {
    }
    
}







@kotlinx.serialization.Serializable
public enum class PaymentDirection {
    
    INBOUND,
    OUTBOUND;
    public companion object
}







@kotlinx.serialization.Serializable
public enum class PaymentFailureReason {
    
    RECIPIENT_REJECTED,
    USER_ABANDONED,
    RETRIES_EXHAUSTED,
    PAYMENT_EXPIRED,
    ROUTE_NOT_FOUND,
    UNEXPECTED_ERROR,
    UNKNOWN_REQUIRED_FEATURES,
    INVOICE_REQUEST_EXPIRED,
    INVOICE_REQUEST_REJECTED,
    BLINDED_PATH_CREATION_FAILED;
    public companion object
}






@kotlinx.serialization.Serializable
public sealed class PaymentKind {
    @kotlinx.serialization.Serializable
    public data class Onchain(
        val `txid`: Txid,
        val `status`: ConfirmationStatus,
    ) : PaymentKind() {
    }
    @kotlinx.serialization.Serializable
    public data class Bolt11(
        val `hash`: PaymentHash,
        val `preimage`: PaymentPreimage?,
        val `secret`: PaymentSecret?,
        val `description`: kotlin.String?,
        val `bolt11`: kotlin.String?,
    ) : PaymentKind() {
    }
    @kotlinx.serialization.Serializable
    public data class Bolt11Jit(
        val `hash`: PaymentHash,
        val `preimage`: PaymentPreimage?,
        val `secret`: PaymentSecret?,
        val `counterpartySkimmedFeeMsat`: kotlin.ULong?,
        val `lspFeeLimits`: LspFeeLimits,
        val `description`: kotlin.String?,
        val `bolt11`: kotlin.String?,
    ) : PaymentKind() {
    }
    @kotlinx.serialization.Serializable
    public data class Bolt12Offer(
        val `hash`: PaymentHash?,
        val `preimage`: PaymentPreimage?,
        val `secret`: PaymentSecret?,
        val `offerId`: OfferId,
        val `payerNote`: UntrustedString?,
        val `quantity`: kotlin.ULong?,
    ) : PaymentKind() {
    }
    @kotlinx.serialization.Serializable
    public data class Bolt12Refund(
        val `hash`: PaymentHash?,
        val `preimage`: PaymentPreimage?,
        val `secret`: PaymentSecret?,
        val `payerNote`: UntrustedString?,
        val `quantity`: kotlin.ULong?,
    ) : PaymentKind() {
    }
    @kotlinx.serialization.Serializable
    public data class Spontaneous(
        val `hash`: PaymentHash,
        val `preimage`: PaymentPreimage?,
    ) : PaymentKind() {
    }
    
}







@kotlinx.serialization.Serializable
public enum class PaymentStatus {
    
    PENDING,
    SUCCEEDED,
    FAILED;
    public companion object
}






@kotlinx.serialization.Serializable
public sealed class PendingSweepBalance {
    @kotlinx.serialization.Serializable
    public data class PendingBroadcast(
        val `channelId`: ChannelId?,
        val `amountSatoshis`: kotlin.ULong,
    ) : PendingSweepBalance() {
    }
    @kotlinx.serialization.Serializable
    public data class BroadcastAwaitingConfirmation(
        val `channelId`: ChannelId?,
        val `latestBroadcastHeight`: kotlin.UInt,
        val `latestSpendingTxid`: Txid,
        val `amountSatoshis`: kotlin.ULong,
    ) : PendingSweepBalance() {
    }
    @kotlinx.serialization.Serializable
    public data class AwaitingThresholdConfirmations(
        val `channelId`: ChannelId?,
        val `latestSpendingTxid`: Txid,
        val `confirmationHash`: BlockHash,
        val `confirmationHeight`: kotlin.UInt,
        val `amountSatoshis`: kotlin.ULong,
    ) : PendingSweepBalance() {
    }
    
}






@kotlinx.serialization.Serializable
public sealed class QrPaymentResult {
    @kotlinx.serialization.Serializable
    public data class Onchain(
        val `txid`: Txid,
    ) : QrPaymentResult() {
    }
    @kotlinx.serialization.Serializable
    public data class Bolt11(
        val `paymentId`: PaymentId,
    ) : QrPaymentResult() {
    }
    @kotlinx.serialization.Serializable
    public data class Bolt12(
        val `paymentId`: PaymentId,
    ) : QrPaymentResult() {
    }
    
}







@kotlinx.serialization.Serializable
public enum class SyncType {
    
    ONCHAIN_WALLET,
    LIGHTNING_WALLET,
    FEE_RATE_CACHE;
    public companion object
}







public sealed class VssHeaderProviderException(message: String): kotlin.Exception(message) {
    
    public class InvalidData(message: String) : VssHeaderProviderException(message)
    
    public class RequestException(message: String) : VssHeaderProviderException(message)
    
    public class AuthorizationException(message: String) : VssHeaderProviderException(message)
    
    public class InternalException(message: String) : VssHeaderProviderException(message)
    
}





@kotlinx.serialization.Serializable
public enum class WordCount {
    
    WORDS12,
    WORDS15,
    WORDS18,
    WORDS21,
    WORDS24;
    public companion object
}











































































































































/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias Address = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias BlockHash = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias ChannelId = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias Lsps1OrderId = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias LspsDateTime = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias Mnemonic = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias NodeAlias = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias NodeId = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias OfferId = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias PaymentHash = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias PaymentId = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias PaymentPreimage = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias PaymentSecret = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias PublicKey = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias SocketAddress = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias Txid = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias UntrustedString = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
public typealias UserChannelId = kotlin.String

