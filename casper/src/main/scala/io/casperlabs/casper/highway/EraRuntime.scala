package io.casperlabs.casper.highway

import cats._
import cats.data.WriterT
import cats.implicits._
import cats.effect.{Clock, Sync}
import cats.effect.concurrent.Ref
import java.time.Instant
import io.casperlabs.casper.consensus.{BlockSummary, Era}
import io.casperlabs.catscontrib.MonadThrowable
import io.casperlabs.crypto.Keys.{PublicKey, PublicKeyBS}
import io.casperlabs.models.Message

/** Class to encapsulate the message handling logic of messages in an era.
  *
  * It can create blocks/ballots and persist them, even new eras,
  * but it should not have externally visible side effects, i.e.
  * not communicate over the network.
  *
  * Persisting messages is fine: if we want to handle messages concurrently
  * we have to keep track of the validator sequence number and make sure
  * we cite our own previous messages. But it's best not to communicate
  * the changes to the outside world right here. We may want to replicate
  * them first in a master-slave environment, in which case we have to
  * make sure the slave has everything persisted as well, so that in the
  * even it has to take over, it won't equivocate.
  *
  * Therefore the handler methods will return a list of domain events,
  * and it's up to the supervisor protocol to send them out. Should
  * make testing easier as well.
  */
class EraRuntime[F[_]: MonadThrowable: Clock](
    conf: HighwayConf,
    val era: Era,
    leaderFunction: LeaderFunction,
    roundExponentRef: Ref[F, Int],
    maybeMessageProducer: Option[MessageProducer[F]]
) {
  import EraRuntime.Agenda, Agenda._
  type HWL[A] = HighwayLog[F, A]

  val startTick = Ticks(era.startTick)
  val endTick   = Ticks(era.endTick)
  val start     = conf.toInstant(startTick)
  val end       = conf.toInstant(endTick)

  val bookingBoundaries =
    conf.criticalBoundaries(start, end, delayDuration = conf.bookingDuration)

  val keyBoundaries =
    conf.criticalBoundaries(start, end, delayDuration = conf.keyDuration)

  /** When we handle an incoming block or create a new one we may have to do additional work:
    * - if the block is a booking block, we have to execute the auction to pick the validators for the upcoming era
    * - when see a switch block, we have to look for a main ancestor which was a key block, to decide which era it belongs to.
    * These blocks can be identified by their round ID being after a boundary (a deadline),
    * while their main parent was still before the deadline.
    */
  private def isBoundary(boundaries: List[Instant])(
      mainParentBlockRoundId: Instant,
      blockRoundId: Instant
  ) = boundaries.exists(t => mainParentBlockRoundId.isBefore(t) && !blockRoundId.isBefore(t))

  val isBookingBoundary = isBoundary(bookingBoundaries)(_, _)
  val isKeyBoundary     = isBoundary(keyBoundaries)(_, _)

  /** Switch blocks are the first blocks which are created _after_ the era ends.
    * They are still created by the validators of this era, and they signal the
    * end of the era. Switch blocks are what the child era is going to build on,
    * however, the validators of _this_ era are the ones that can finalize it by
    * building ballots on top of it. They cannot build more blocks on them though.
    */
  val isSwitchBoundary = (mpbr: Instant, br: Instant) => mpbr.isBefore(end) && !br.isBefore(end)

  /** Whether the validator is bonded depends on the booking block. Only bonded validators
    * have to produce blocks and ballots in the era.
    * TODO: Decide if unbonded validators need to maintain round statistics about finalized lambda messages.
    */
  private def withProducer[G[_], A](default: G[A])(f: MessageProducer[F] => G[A]): G[A] =
    maybeMessageProducer match {
      case Some(mp) if era.bonds.exists(b => b.validatorPublicKey == mp.validatorId) =>
        f(mp)
      case _ =>
        default
    }

  private def currentTick =
    Clock[F].realTime(conf.tickUnit).map(Ticks(_))

  /** Check if the era is finished yet, including the post-era voting period. */
  private def isOverAt(tick: Ticks): F[Boolean] =
    if (tick < endTick) false.pure[F]
    else {
      conf.postEraVotingDuration match {
        case HighwayConf.VotingDuration.FixedLength(duration) =>
          (end plus duration).isBefore(conf.toInstant(tick)).pure[F]

        case HighwayConf.VotingDuration.SummitLevel(_) =>
          ???
      }
    }

  private def roundLength: F[Ticks] =
    // TODO: We should be able to tell about each past tick what the exponent at the time was.
    roundExponentRef.get.map(Ticks.roundLength(_))

  /** Calculate the beginnin and the end of this round,
    * based on the current tick and the round length. */
  private def roundBoundariesAt(tick: Ticks): F[(Ticks, Ticks)] =
    roundLength map { length =>
      val start = if (tick % length == 0) tick else Ticks(tick + length - tick % length)
      start -> Ticks(start + length)
    }

  private def createLambdaResponse(
      messageProducer: MessageProducer[F],
      lambdaMessage: Message.Block
  ): HWL[Unit] =
    // TODO: Create persistent block.
    for {
      b <- HighwayLog.liftF {
            messageProducer.ballot(
              target = lambdaMessage.messageHash,
              // TODO: Get our own latest message. Get the latest message from the storage, in case they equivocated.
              justifications =
                Map(PublicKey(lambdaMessage.validatorId) -> Set(lambdaMessage.messageHash)),
              roundId = Ticks(lambdaMessage.roundId)
            )
          }
      _ <- HighwayLog.tell[F](
            HighwayEvent.CreatedLambdaResponse(b)
          )
    } yield ()

  /** Produce a starting agenda, depending on whether the validator is bonded or not. */
  def initAgenda: F[Agenda] =
    withProducer(Agenda.empty[F]) { _ =>
      currentTick flatMap { tick =>
        isOverAt(tick).ifM(
          Agenda.empty[F],
          roundBoundariesAt(tick) flatMap {
            case (from, to) =>
              val roundId = if (from >= tick) from else to
              Agenda[F](roundId -> StartRound(roundId))
          }
        )
      }
    }

  /** Handle a block or ballot coming from another validator. For example:
    * - if it's a lambda message, create a lambda response
    * - if it's a switch block, create a new era, unless it exists already.
    * Returns a list of events that happened during the persisting of changes.
    * This method is always called as a reaction to an incoming message,
    * so it doesn't return a future agenda of its own.
    */
  def handleMessage(message: Message): HWL[Unit] =
    message match {
      case _: Message.Ballot =>
        HighwayLog.unit

      case b: Message.Block =>
        val roundId = Ticks(b.roundId)
        withProducer(HighwayLog.unit[F]) { mp =>
          if (mp.validatorId == b.validatorId) {
            MonadThrowable[HWL].raiseError(
              new IllegalStateException("Shouldn't receive our own messages!")
            )
          } else if (b.keyBlockHash != era.keyBlockHash) {
            MonadThrowable[HWL].raiseError(
              new IllegalStateException("Shouldn't receive messages from other eras!")
            )
          } else if (leaderFunction(roundId) != b.validatorId) {
            HighwayLog.unit
          } else {
            // TODO: Verify if it's okay not to send a response to a message where we *did*
            // participate in the round it belongs to, but we moved on to a newer round.
            HighwayLog.liftF(currentTick.flatMap(roundBoundariesAt)).flatMap {
              case (from, _) if from == roundId =>
                createLambdaResponse(mp, b)
              case _ =>
                HighwayLog.unit
            }
          }
        }
    }

  /** Handle something that happens during a round:
    * - in rounds when we are leading, create a lambda message
    * - if it's a booking block, execute the auction
    * - if the main parent is a switch block, create a ballot instead
    * - midway through the round, create an omega message
    * - if we're beyond the voting period after the end of the era, stop.
    * Returns a list of events that took place, as well as its future agenda,
    * telling the caller when it has to be scheduled again.
    */
  def handleAgenda(action: Agenda.Action): HighwayLog[F, Agenda] = ???

}

object EraRuntime {

  def fromGenesis[F[_]: Sync: Clock](
      conf: HighwayConf,
      genesis: BlockSummary,
      maybeMessageProducer: Option[MessageProducer[F]],
      initRoundExponent: Int,
      leaderSequencer: LeaderSequencer = LeaderSequencer
  ): F[EraRuntime[F]] = {
    val era = Era(
      keyBlockHash = genesis.blockHash,
      bookingBlockHash = genesis.blockHash,
      startTick = conf.toTicks(conf.genesisEraStart),
      endTick = conf.toTicks(conf.genesisEraEnd),
      bonds = genesis.getHeader.getState.bonds
    )
    for {
      leaderFunction   <- leaderSequencer[F](era)
      roundExponentRef <- Ref.of[F, Int](initRoundExponent)
    } yield {
      new EraRuntime[F](
        conf,
        era,
        leaderFunction,
        roundExponentRef,
        maybeMessageProducer
      )
    }
  }

  /** List of future actions to take. */
  type Agenda = Vector[Agenda.DelayedAction]

  object Agenda {
    sealed trait Action

    /** What action to take and when. */
    case class DelayedAction(tick: Ticks, action: Action)

    /** Handle one round:
      * - in rounds when we are leading, create a lambda message
      * - if it's a booking block, execute the auction
      * - if the main parent is a switch block, create a ballot instead
      * - if we're beyond the voting period after the end of the era, stop.
      */
    case class StartRound(roundId: Ticks) extends Action

    /** Create an Omega message, some time during the round */
    case class CreateOmegaMessage(roundId: Ticks) extends Action

    def apply[F[_]: Applicative](
        actions: (Ticks, Action)*
    ): F[Agenda] =
      actions.map(DelayedAction.tupled).toVector.pure[F]

    def empty[F[_]: Applicative] = apply[F]()
  }
}
